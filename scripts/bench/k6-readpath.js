// k6 load test for the read-path HTTP API.
//
// Run via the wrapper script (`scripts/bench-http.sh`), which spins
// the server up on a known port and points k6 at it.
//
// One scenario per endpoint shape so each one has a clean signal.
// All scenarios run sequentially (`startTime` staggered) so they
// don't share CPU; if you want them in parallel, drop the
// startTime offsets.
//
// Output: JSON summary on stdout (handleSummary). Side-by-side runs
// can diff p50 / p95 / rps from the JSON; the wrapper script does
// this for "before vs after" comparisons.

import http from 'k6/http';
import { check, fail } from 'k6';
import { Trend, Counter } from 'k6/metrics';

// AU fixtures — the local dev index is AU-only. Mix of urban /
// suburban / rural / no-hit so the bench surfaces hot and cold paths.
const COORDS = [
    { name: 'sydney_cbd', lat: -33.8688, lon: 151.2093 },
    { name: 'melbourne_cbd', lat: -37.8136, lon: 144.9631 },
    { name: 'brisbane_cbd', lat: -27.4698, lon: 153.0251 },
    { name: 'sydney_parramatta', lat: -33.815, lon: 151.0011 },
    { name: 'regional_orange', lat: -33.2833, lon: 149.1 },
    { name: 'tasman_sea', lat: -35.0, lon: 155.0 },
];

const SEARCH_QUERIES = [
    'sydney',         // FST fast-path hit
    'melbourne',      // FST fast-path hit
    'bondi beach',    // multi-token, FST hit
    'elizabeth street', // street + token, tantivy
    'parramatta road',  // street, tantivy
];

const AUTOCOMPLETE_PREFIXES = [
    's',          // very broad — heap-allocation hot path
    'sy',         // broad
    'syd',        // medium
    'sydn',       // narrow
    'sydney',     // full word
    'bondi',      // suburb prefix
];

// Custom metrics so handleSummary can extract per-endpoint p50 / p95.
const reverseLatency = new Trend('reverse_ms', true);
const reverseLangLatency = new Trend('reverse_lang_ms', true);
const searchLatency = new Trend('search_ms', true);
const autocompleteLatency = new Trend('autocomplete_ms', true);
const autocompleteBroadLatency = new Trend('autocomplete_broad_ms', true);
const errors = new Counter('endpoint_errors');

// Scenario settings — each holds 8 VUs for 20s after a 5s warm-up.
// Keeps the run under 3 minutes total.
const VUS = 8;
const DURATION = '20s';
const WARMUP_DURATION = '5s';

export const options = {
    scenarios: {
        reverse: {
            executor: 'constant-vus',
            vus: VUS,
            duration: DURATION,
            startTime: '5s',
            exec: 'reverse',
        },
        reverse_lang: {
            executor: 'constant-vus',
            vus: VUS,
            duration: DURATION,
            startTime: '30s',
            exec: 'reverseLang',
        },
        search: {
            executor: 'constant-vus',
            vus: VUS,
            duration: DURATION,
            startTime: '55s',
            exec: 'search',
        },
        autocomplete_broad: {
            executor: 'constant-vus',
            vus: VUS,
            duration: DURATION,
            startTime: '80s',
            exec: 'autocompleteBroad',
        },
        autocomplete_mixed: {
            executor: 'constant-vus',
            vus: VUS,
            duration: DURATION,
            startTime: '105s',
            exec: 'autocompleteMixed',
        },
    },
    // Targets — the bench reports actuals; these are sanity rails so
    // a regression that 10×s a tail latency turns the run red.
    thresholds: {
        http_req_failed: ['rate<0.01'],
        reverse_ms: ['p(95)<5'],
        autocomplete_ms: ['p(95)<10'],
        autocomplete_broad_ms: ['p(95)<25'],
    },
    // Don't print per-iter details — final summary only.
    summaryTimeUnit: 'ms',
    // Default Trend stats are min/avg/med/p(90)/p(95)/max — add p(99)
    // and count so the custom handler can render them.
    summaryTrendStats: ['min', 'avg', 'med', 'p(90)', 'p(95)', 'p(99)', 'max', 'count'],
};

const BASE = __ENV.BASE_URL || 'http://localhost:13580';

function pick(arr) {
    return arr[Math.floor(Math.random() * arr.length)];
}

function checkOk(resp, label) {
    const ok = check(resp, {
        [`${label}: 2xx`]: (r) => r.status >= 200 && r.status < 300,
    });
    if (!ok) {
        errors.add(1);
    }
    return ok;
}

export function reverse() {
    const c = pick(COORDS);
    const url = `${BASE}/reverse?lat=${c.lat}&lon=${c.lon}`;
    const resp = http.get(url, { tags: { endpoint: 'reverse' } });
    reverseLatency.add(resp.timings.duration);
    checkOk(resp, 'reverse');
}

export function reverseLang() {
    const c = pick(COORDS);
    // lang=zh exercises the i18n_names lookup path that perf win #1
    // (find_admin dedup) targets. Pre-optimisation: ~50 µs of double
    // admin scan per request. Post: single scan.
    const url = `${BASE}/reverse?lat=${c.lat}&lon=${c.lon}&lang=zh`;
    const resp = http.get(url, { tags: { endpoint: 'reverse_lang' } });
    reverseLangLatency.add(resp.timings.duration);
    checkOk(resp, 'reverse_lang');
}

export function search() {
    const q = pick(SEARCH_QUERIES);
    const url = `${BASE}/search?q=${encodeURIComponent(q)}&country_code=au`;
    const resp = http.get(url, { tags: { endpoint: 'search' } });
    searchLatency.add(resp.timings.duration);
    checkOk(resp, 'search');
}

export function autocompleteBroad() {
    // Always 1–2 char prefix. This is where the deferred-string-
    // hydration win is most visible — pre-optimisation, every visited
    // FST candidate (up to FST_WALK_CAP=10000) allocated a `String`.
    const q = pick(['s', 'sy', 'b', 'm', 'br']);
    const url = `${BASE}/autocomplete?q=${encodeURIComponent(q)}&country_code=au&limit=10`;
    const resp = http.get(url, { tags: { endpoint: 'autocomplete_broad' } });
    autocompleteBroadLatency.add(resp.timings.duration);
    checkOk(resp, 'autocomplete_broad');
}

export function autocompleteMixed() {
    // Real-world typeahead: a mix of prefix lengths simulating a user
    // typing. Picks one prefix per VU iteration uniformly.
    const q = pick(AUTOCOMPLETE_PREFIXES);
    const url = `${BASE}/autocomplete?q=${encodeURIComponent(q)}&country_code=au&limit=10`;
    const resp = http.get(url, { tags: { endpoint: 'autocomplete_mixed' } });
    autocompleteLatency.add(resp.timings.duration);
    checkOk(resp, 'autocomplete_mixed');
}

// Custom summary handler: emits a compact JSON summary that the
// wrapper script can diff between runs. Default k6 summary is human-
// friendly but hard to parse.
//
// k6 calls this once at the end of the test and replaces the default
// summary entirely with whatever it returns. If we throw or return
// the wrong shape, k6 falls back silently — so keep it defensive.
export function handleSummary(data) {
    try {
        const m = data && data.metrics ? data.metrics : {};
        const pickTrend = (key) => {
            const t = m[key];
            if (!t || !t.values) return null;
            const v = t.values;
            return {
                count: v.count || 0,
                min: v.min || 0,
                avg: v.avg || 0,
                med: v.med || 0,
                p90: v['p(90)'] || 0,
                p95: v['p(95)'] || 0,
                p99: v['p(99)'] || 0,
                max: v.max || 0,
            };
        };
        const trends = {};
        for (const k of [
            'reverse_ms',
            'reverse_lang_ms',
            'search_ms',
            'autocomplete_ms',
            'autocomplete_broad_ms',
        ]) {
            const t = pickTrend(k);
            if (t) trends[k] = t;
        }
        const summary = {
            endpoints: trends,
            http_reqs: m.http_reqs && m.http_reqs.values ? m.http_reqs.values.count : 0,
            http_req_rate: m.http_reqs && m.http_reqs.values ? m.http_reqs.values.rate : 0,
            errors: m.endpoint_errors && m.endpoint_errors.values ? m.endpoint_errors.values.count : 0,
        };
        // The wrapper runs us once per mode (cold / warm) and merges
        // the per-mode summaries afterwards. MODE_TAG lets the
        // wrapper distinguish them; defaults to "all" for ad-hoc
        // single-shot runs that bypass the wrapper.
        const tag = (__ENV.MODE_TAG || 'all').replace(/[^a-z0-9_-]/gi, '');
        return {
            stdout: formatTable(summary, tag),
            // Mount-friendly out-path. /tmp inside the k6 container
            // isn't writable under the non-root k6 user without an
            // explicit mount of the right shape.
            [`/out/k6-summary-${tag}.json`]: JSON.stringify(summary, null, 2),
        };
    } catch (e) {
        // If anything in handleSummary throws, k6 silently falls back
        // to the default summary. Surface the error so the operator
        // can fix the JS, not chase a missing-summary file.
        return {
            stdout: `handleSummary error: ${e}\n`,
        };
    }
}

function formatTable(s, tag) {
    const lines = [];
    lines.push('');
    lines.push(`===== HTTP read-path bench (mode=${tag || 'all'}) =====`);
    const rps = (s.http_req_rate || 0).toFixed(0);
    lines.push(`Total requests: ${s.http_reqs}   rps: ${rps}   errors: ${s.errors}`);
    lines.push('');
    lines.push('endpoint                  p50      p90      p95      p99      max     count');
    lines.push('------------------------ -------- -------- -------- -------- -------- --------');
    for (const [k, v] of Object.entries(s.endpoints || {})) {
        const fmt = (n) => `${(n || 0).toFixed(2)}ms`.padStart(8);
        lines.push(
            `${k.padEnd(24)} ${fmt(v.med)} ${fmt(v.p90)} ${fmt(v.p95)} ${fmt(v.p99)} ${fmt(v.max)} ${String(v.count).padStart(8)}`,
        );
    }
    lines.push('');
    return lines.join('\n');
}
