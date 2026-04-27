# SDK patterns

The geocoder ships no client SDK. The wire format is plain JSON over HTTP plus protobuf over gRPC; both are easy to consume from any language. This doc collects idiomatic snippets for the cases that benefit from a small client-side wrapper — most importantly, **proximity bias for ambiguous-name queries**.

## Why no SDK in this repo

- The HTTP API is six endpoints. Hand-rolled clients are usually 30 lines.
- Bundling and versioning a TS / Python / Go / Swift / Kotlin SDK is months of maintenance and a CI surface we don't want yet.
- If your usage shows real demand, fork these snippets into a thin wrapper inside your own monorepo.

## Proximity bias: where the coord comes from

`/search?bias_lat=…&bias_lng=…` (and the equivalent gRPC `SearchRequest.bias_lat`/`bias_lng`) takes a user-location coord and re-ranks results so locally-relevant matches outrank globally-prominent ones with the same BM25 score. The geocoder doesn't care where the coord comes from — that's the SDK's job. Pick the highest-quality signal you have.

| Source | Accuracy | When to use |
|---|---|---|
| Browser `navigator.geolocation` | ~10 m (with permission), city-level (without) | Web app, opt-in geolocation prompt acceptable |
| Mobile GPS (CoreLocation / FusedLocationProvider) | ~5–20 m | Native mobile, location permission granted |
| Cached user-profile coord | as good as your data | Backend with logged-in user, profile carries home/work coord |
| IP-to-coord via `/geocode/ip` | country–city level (60–90 % city accuracy on consumer ISPs) | Backend that only has the request IP; fallback when nothing better |
| **Don't pass any bias** | n/a | Default. Geocoder returns the globally-prominent match. |

## Snippets

### Browser (TypeScript)

```typescript
async function search(query: string): Promise<SearchResponse> {
  const params = new URLSearchParams({ q: query, limit: '10' });

  // Best-effort: ask the browser for a coord. If the user denies or
  // the API isn't available, fall through to an unbiased query.
  if ('geolocation' in navigator) {
    try {
      const pos = await new Promise<GeolocationPosition>((resolve, reject) => {
        navigator.geolocation.getCurrentPosition(resolve, reject, {
          maximumAge: 60_000,    // accept a 1-min-old fix
          timeout: 1_500,        // don't block the search on a slow GPS
        });
      });
      params.set('bias_lat', pos.coords.latitude.toString());
      params.set('bias_lng', pos.coords.longitude.toString());
    } catch {
      // Permission denied or timeout — query without bias.
    }
  }

  const res = await fetch(`${GEOCODER_BASE}/search?${params}`);
  return res.json();
}
```

Notes:
- Use `maximumAge` so you're not blocking every search on a fresh GPS fix.
- `timeout: 1500ms` keeps the request snappy when the browser can't get a fix quickly.
- Don't surprise the user with a permission prompt — gate behind explicit opt-in.

### iOS (Swift)

```swift
import CoreLocation

func search(_ query: String, completion: @escaping (Result<SearchResponse, Error>) -> Void) {
    var components = URLComponents(string: "\(geocoderBase)/search")!
    components.queryItems = [
        URLQueryItem(name: "q", value: query),
        URLQueryItem(name: "limit", value: "10"),
    ]

    // Use the most recent location the app already has. Don't trigger
    // a fresh fix here — the search would block on it. Background
    // location updates should keep `locationManager.location` warm.
    if let coord = locationManager.location?.coordinate {
        components.queryItems?.append(URLQueryItem(name: "bias_lat", value: "\(coord.latitude)"))
        components.queryItems?.append(URLQueryItem(name: "bias_lng", value: "\(coord.longitude)"))
    }

    URLSession.shared.dataTask(with: components.url!) { data, _, error in
        // ... decode SearchResponse
    }.resume()
}
```

### Android (Kotlin)

```kotlin
import com.google.android.gms.location.LocationServices

suspend fun search(query: String): SearchResponse {
    val builder = HttpUrl.Builder()
        .scheme("https").host("geocoder.example.com")
        .addPathSegment("search")
        .addQueryParameter("q", query)
        .addQueryParameter("limit", "10")

    // FusedLocationProviderClient.lastLocation gives the cached fix —
    // doesn't trigger a new GPS read, so it returns immediately.
    val loc = LocationServices.getFusedLocationProviderClient(context)
        .lastLocation.await()
    if (loc != null) {
        builder.addQueryParameter("bias_lat", loc.latitude.toString())
        builder.addQueryParameter("bias_lng", loc.longitude.toString())
    }

    return httpClient.get(builder.build()).body()
}
```

### Backend with cached user coord (Go)

```go
type User struct {
    HomeLat *float64 // nullable — opt-in profile field
    HomeLng *float64
}

func search(ctx context.Context, user User, query string) (*SearchResponse, error) {
    q := url.Values{}
    q.Set("q", query)
    q.Set("limit", "10")

    // If the user's profile carries coords, use them.
    if user.HomeLat != nil && user.HomeLng != nil {
        q.Set("bias_lat", fmt.Sprintf("%.6f", *user.HomeLat))
        q.Set("bias_lng", fmt.Sprintf("%.6f", *user.HomeLng))
    }

    resp, err := http.Get(geocoderBase + "/search?" + q.Encode())
    // ... decode
}
```

### Backend with only request IP (Python)

When you don't have a profile coord, chain through `/geocode/ip` to resolve the caller's IP, then use that as the bias. **Cache the IP→coord result** — the IP geo lookup is mmap'd MaxMind, but a single user makes many search calls.

```python
import functools
import requests

GEOCODER_BASE = "https://geocoder.example.com"

@functools.lru_cache(maxsize=10_000)
def ip_to_coord(ip: str) -> tuple[float, float] | None:
    """Cache IP→coord; fall back to None if the IP isn't in the database."""
    try:
        r = requests.get(f"{GEOCODER_BASE}/geocode/ip", params={"ip": ip}, timeout=0.5)
        if r.status_code != 200:
            return None
        body = r.json()
        return (body["lat"], body["lon"])
    except requests.RequestException:
        return None

def search(query: str, *, client_ip: str | None = None) -> dict:
    params = {"q": query, "limit": 10}
    if client_ip:
        coord = ip_to_coord(client_ip)
        if coord:
            params["bias_lat"], params["bias_lng"] = coord
    return requests.get(f"{GEOCODER_BASE}/search", params=params, timeout=2).json()

# Usage in a request handler — pull X-Forwarded-For or peer IP, pass to search().
result = search("Cambridge", client_ip=request.headers.get("X-Forwarded-For", "").split(",")[0].strip())
```

Notes:
- `lru_cache(maxsize=10_000)` keeps ~10k recently-seen IPs warm; tune to your traffic.
- The IP→coord call is a separate round-trip; budget ~1–5 ms on a healthy network. If that's too much, run a MaxMind reader in-process (every language has one) and skip `/geocode/ip` entirely.
- Don't log the IP unless you've considered the GDPR / CCPA implications. The geocoder doesn't log it by default.

## Privacy

`bias_lat`/`bias_lng` are coords, not IPs — no PII passes the API boundary. If you use IP-to-coord on the backend, the IP stays on your side. The geocoder logs request lines including query parameters at debug level only; in production, set `RUST_LOG=info` (the default) and the bias coord doesn't appear in logs.

If you need to *not* log even at the application layer, run the geocoder behind a reverse proxy that strips query strings from access logs.

## What about the FST fast-path?

The `/search` FST fast-path returns one globally-prominent match in ~400 ns. It activates when the query is a simple freeform string AND `limit==1` AND no bias is supplied. As soon as you add `bias_lat`/`bias_lng`, the request goes through Tantivy (~50 µs at p50). Don't pass bias if you have no signal — the round-trip cost is real, and an unbiased query still gives a sensible answer.

## gRPC

The same patterns apply — `SearchRequest.bias_lat` / `bias_lng` are proto3 `optional` doubles. Set both or neither. Out-of-range values return `Status::InvalidArgument`. See `server/proto/geocoder.proto` for the full message shape.
