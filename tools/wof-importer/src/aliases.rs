//! Export conservative WoF city short names ("NYC", "San Fran", "Philly")
//! as `wof_aliases.tsv` for `build-forward-index`.
//!
//! WoF English names mix real short forms with nicknames ("Fog City",
//! "The Big Apple") and codes ("LAX", "PHL"). An alias is kept only when
//! it is derived from one of the place's own names by a rule below.
//! The schema is documented in docs/binary-format.md.

use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const HEADER: &str = "#wof-aliases-v1\tcountry\tname\tlatitude\tlongitude\talias\trule";

/// WoF records of one city (locality + localadmin, duplicate ids across
/// files) closer than this count as one place in a collision.
const SAME_PLACE_KM: f64 = 10.0;
/// A colliding alias stays on the largest claimant only when it has at
/// least this many times the population of the next one.
const DOMINANCE_RATIO: i64 = 10;
/// Initialisms are ambiguous ("AC", "GF"); only a large city owns one.
const INITIALISM_MIN_POPULATION: i64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    Initialism,
    Compact,
    Abbreviated,
    Shortened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reject {
    LeadingThe,
    Code,
    NoRule,
    SmallPlace,
    Collision,
}

impl Rule {
    fn label(self) -> &'static str {
        match self {
            Rule::Initialism => "initialism",
            Rule::Compact => "compact",
            Rule::Abbreviated => "abbreviated",
            Rule::Shortened => "shortened",
        }
    }
}

/// Remove dots and collapse whitespace: "N.Y.C." -> "NYC", "S. Fran" -> "S Fran".
fn tidy(alias: &str) -> String {
    let mut out = String::new();
    let mut in_initials = false;
    for word in alias.replace('.', ". ").split_whitespace() {
        let initial =
            word.len() == 2 && word.ends_with('.') && word.as_bytes()[0].is_ascii_uppercase();
        let word = word.trim_end_matches('.');
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() && !(initial && in_initials) {
            out.push(' ');
        }
        out.push_str(word);
        in_initials = initial;
    }
    out
}

fn words(name: &str) -> Vec<String> {
    name.split(|c: char| c.is_whitespace() || c == '-')
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn initials(words: &[String]) -> String {
    words
        .iter()
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

fn compact(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Decide whether `alias` (already passed through [`tidy`]) is a short
/// form of one of `canonical`, the place's own names.
fn classify(alias: &str, canonical: &[&str]) -> Result<Rule, Reject> {
    if alias.to_lowercase().starts_with("the ") {
        return Err(Reject::LeadingThe);
    }
    if alias.chars().any(|c| c.is_ascii_digit()) {
        return Err(Reject::NoRule);
    }
    let alias_words: Vec<&str> = alias.split(' ').collect();
    let upper = alias.chars().all(|c| c.is_ascii_uppercase());
    for name in canonical {
        let name_words = words(name);
        if name_words.len() >= 2 && upper && alias.len() >= 2 {
            let init = initials(&name_words);
            if alias == init || alias.strip_suffix('C') == Some(init.as_str()) {
                return Ok(Rule::Initialism);
            }
        }
        if name_words.len() >= 2
            && alias.chars().all(char::is_alphabetic)
            && compact(alias) == compact(name)
        {
            return Ok(Rule::Compact);
        }
        if abbreviates(&alias_words, &name_words) {
            return Ok(Rule::Abbreviated);
        }
        if alias_words.len() == 1 && shortens(alias, name, &name_words) {
            return Ok(Rule::Shortened);
        }
    }
    // Three to five capitals that are not the name's initials are
    // airport, station or team codes (LAX, PHL, YVR, MNYSC).
    if upper && (3..=5).contains(&alias.len()) {
        return Err(Reject::Code);
    }
    Err(Reject::NoRule)
}

/// Each alias word is the name word itself, a 3+ letter prefix of it
/// that drops 3+ letters ("Fran", "Sta"; not "Garden" for "Gardens" or
/// a cut-off "Brentwo"), or the capital initials of 2+ consecutive name
/// words ("NY" for "New York"). At least one word is kept and one is
/// shortened.
fn abbreviates(alias_words: &[&str], name_words: &[String]) -> bool {
    if alias_words.len() < 2 {
        return false;
    }
    let (mut at, mut kept, mut shortened) = (0, false, false);
    for word in alias_words {
        let Some(name_word) = name_words.get(at) else {
            return false;
        };
        if word.eq_ignore_ascii_case(name_word) {
            kept = true;
            at += 1;
            continue;
        }
        let len = word.chars().count();
        let upper = word.chars().all(|c| c.is_ascii_uppercase());
        if upper
            && len >= 2
            && at + len <= name_words.len()
            && initials(&name_words[at..at + len]) == *word
        {
            shortened = true;
            at += len;
            continue;
        }
        if len >= 3
            && len + 3 <= name_word.chars().count()
            && name_word.to_lowercase().starts_with(&word.to_lowercase())
        {
            shortened = true;
            at += 1;
            continue;
        }
        return false;
    }
    at == name_words.len() && kept && shortened
}

/// A single word that keeps the first 4+ letters of the name and adds a
/// diminutive ending, with an optional doubled letter: "Philly"
/// (Phil-l-y), "Brissie", "Welly", "Berko". A bare cut ("Park" for
/// Parkville, "Jersey", "Oakland" for Oaklands) and a whole word of the
/// name ("Stratford", "Newcastle") name other places too.
fn shortens(alias: &str, name: &str, name_words: &[String]) -> bool {
    const ENDINGS: &[&str] = &["y", "ie", "o"];
    let alias = alias.to_lowercase();
    let full = compact(name);
    if !alias.chars().all(char::is_alphabetic)
        || alias.len() >= full.len()
        || name_words.iter().any(|w| w.to_lowercase() == alias)
    {
        return false;
    }
    let common = alias
        .bytes()
        .zip(full.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if common < 4 || !alias.is_char_boundary(common) {
        return false;
    }
    let rest = &alias[common..];
    let doubled = alias[..common]
        .chars()
        .last()
        .and_then(|last| rest.strip_prefix(last));
    ENDINGS.contains(&rest) || doubled.is_some_and(|tail| ENDINGS.contains(&tail))
}

/// Place index, the rule that kept the alias, and the tidied alias.
type Claim = (usize, Rule, String);

struct Place {
    db: usize,
    id: i64,
    name: String,
    country: String,
    lat: f64,
    lng: f64,
    canonical: Vec<String>,
    candidates: Vec<String>,
}

#[derive(Default)]
struct Counts {
    kept: BTreeMap<Rule, u64>,
    dropped: BTreeMap<Reject, u64>,
}

pub fn import_aliases(sqlite_dir: &Path, out_dir: &Path) -> Result<(), String> {
    let mut dbs: Vec<PathBuf> = fs::read_dir(sqlite_dir)
        .map_err(|e| format!("read_dir {}: {e}", sqlite_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("db")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("whosonfirst-data-admin-"))
        })
        .collect();
    dbs.sort();
    if dbs.is_empty() {
        eprintln!("no WoF admin SQLite files; skipping alias export");
        return Ok(());
    }
    let conns = dbs
        .iter()
        .map(|p| Connection::open(p).map_err(|e| format!("open {}: {e}", p.display())))
        .collect::<Result<Vec<_>, _>>()?;

    let mut places: Vec<Place> = Vec::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();
    for (db, conn) in conns.iter().enumerate() {
        let mut by_id: HashMap<i64, usize> = HashMap::new();
        let mut stmt = conn
            .prepare(
                "SELECT s.id, s.name, s.country, s.latitude, s.longitude, n.name, n.privateuse \
                 FROM spr s LEFT JOIN names n ON n.id = s.id AND n.language = 'eng' \
                 WHERE s.placetype IN ('locality', 'localadmin', 'borough') \
                 AND s.is_current = 1 AND s.is_deprecated = 0 AND s.is_ceased = 0 \
                 AND s.is_superseded = 0 ORDER BY s.id",
            )
            .map_err(|e| format!("prepare {}: {e}", dbs[db].display()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|e| format!("query {}: {e}", dbs[db].display()))?;
        for row in rows {
            let (id, name, country, lat, lng, alt, privateuse) =
                row.map_err(|e| format!("row {}: {e}", dbs[db].display()))?;
            let (Some(name), Some(country), Some(lat), Some(lng)) = (name, country, lat, lng)
            else {
                continue;
            };
            if name.is_empty() || country.len() != 2 || (lat == 0.0 && lng == 0.0) {
                continue;
            }
            let slot = match by_id.get(&id) {
                Some(&slot) => slot,
                None => {
                    if !seen_ids.insert(id) {
                        continue;
                    }
                    places.push(Place {
                        db,
                        id,
                        canonical: vec![name.clone()],
                        name,
                        country,
                        lat,
                        lng,
                        candidates: Vec::new(),
                    });
                    by_id.insert(id, places.len() - 1);
                    places.len() - 1
                }
            };
            let place = &mut places[slot];
            let Some(alt) = alt.filter(|a| !a.contains(['\t', '\n', '\r'])) else {
                continue;
            };
            if privateuse.as_deref() == Some("preferred") {
                if !place.canonical.iter().any(|c| c.eq_ignore_ascii_case(&alt)) {
                    place.canonical.push(alt);
                }
            } else {
                place.candidates.push(alt);
            }
        }
    }

    let mut counts: BTreeMap<String, Counts> = BTreeMap::new();
    // (country, lowercase alias) -> claimants (place index, rule, alias)
    let mut claims: HashMap<(String, String), Vec<Claim>> = HashMap::new();
    for (i, place) in places.iter().enumerate() {
        let canonical: Vec<&str> = place.canonical.iter().map(String::as_str).collect();
        let mut done: HashSet<String> = canonical.iter().map(|c| c.to_lowercase()).collect();
        let tally = counts.entry(place.country.clone()).or_default();
        for raw in &place.candidates {
            let alias = tidy(raw);
            if alias.is_empty() || !done.insert(alias.to_lowercase()) {
                continue;
            }
            match classify(&alias, &canonical) {
                Ok(rule) => claims
                    .entry((place.country.clone(), alias.to_lowercase()))
                    .or_default()
                    .push((i, rule, alias)),
                Err(reason) => *tally.dropped.entry(reason).or_default() += 1,
            }
        }
    }

    // Another place whose own name is the alias also claims it.
    let mut named: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, place) in places.iter().enumerate() {
        for name in &place.canonical {
            let key = (place.country.clone(), tidy(name).to_lowercase());
            if claims.contains_key(&key) {
                named.entry(key).or_default().push(i);
            }
        }
    }

    let mut population_cache: HashMap<i64, i64> = HashMap::new();
    let mut kept: Vec<(usize, Rule, String)> = Vec::new();
    let mut keys: Vec<_> = claims.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let claimants = claims.remove(&key).expect("key came from the map");
        let mut people: Vec<usize> = claimants.iter().map(|(i, _, _)| *i).collect();
        people.extend(named.get(&key).into_iter().flatten().copied());
        let groups = same_place_groups(&places, &people);
        let winner = if groups.len() == 1 {
            Some(0)
        } else {
            let mut sized: Vec<(i64, usize)> = Vec::with_capacity(groups.len());
            for (g, members) in groups.iter().enumerate() {
                let mut pop = 0;
                for &i in members {
                    pop = pop.max(population(&conns, &places[i], &mut population_cache)?);
                }
                sized.push((pop, g));
            }
            sized.sort_by(|a, b| b.cmp(a));
            (sized[0].0 > 0 && sized[0].0 >= sized[1].0.saturating_mul(DOMINANCE_RATIO))
                .then_some(sized[0].1)
        };
        for (i, rule, alias) in claimants {
            let tally = counts.entry(places[i].country.clone()).or_default();
            if rule == Rule::Initialism
                && population(&conns, &places[i], &mut population_cache)?
                    < INITIALISM_MIN_POPULATION
            {
                *tally.dropped.entry(Reject::SmallPlace).or_default() += 1;
            } else if winner.is_some_and(|g| groups[g].contains(&i)) {
                *tally.kept.entry(rule).or_default() += 1;
                kept.push((i, rule, alias));
            } else {
                *tally.dropped.entry(Reject::Collision).or_default() += 1;
            }
        }
    }
    kept.sort_by(|a, b| {
        (&places[a.0].country, &places[a.0].name, &a.2).cmp(&(
            &places[b.0].country,
            &places[b.0].name,
            &b.2,
        ))
    });

    fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;
    let dest = out_dir.join("wof_aliases.tsv");
    let tmp = out_dir.join("wof_aliases.tsv.tmp");
    let file = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "{HEADER}").map_err(|e| format!("write {}: {e}", tmp.display()))?;
    let mut rows = 0u64;
    for (i, rule, alias) in &kept {
        let place = &places[*i];
        for name in &place.canonical {
            writeln!(
                writer,
                "{}\t{}\t{}\t{}\t{}\t{}",
                place.country,
                name,
                place.lat,
                place.lng,
                alias,
                rule.label()
            )
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            rows += 1;
        }
    }
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &dest)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), dest.display()))?;

    for (country, tally) in &counts {
        let kept: Vec<String> = tally
            .kept
            .iter()
            .map(|(r, n)| format!("{}={n}", r.label()))
            .collect();
        let dropped: Vec<String> = tally
            .dropped
            .iter()
            .map(|(r, n)| format!("{r:?}={n}"))
            .collect();
        eprintln!(
            "aliases {country}: kept [{}] dropped [{}]",
            kept.join(" "),
            dropped.join(" ")
        );
    }
    eprintln!(
        "wrote {} alias rows ({} aliases) to {}",
        rows,
        kept.len(),
        dest.display()
    );
    Ok(())
}

/// Group claimants that are the same city: same name, within
/// [`SAME_PLACE_KM`]. Index 0 of the result holds the first claimant.
fn same_place_groups(places: &[Place], people: &[usize]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &i in people {
        let p = &places[i];
        let home = groups.iter_mut().find(|g| {
            g.iter().any(|&j| {
                let q = &places[j];
                j == i
                    || (p.name.eq_ignore_ascii_case(&q.name)
                        && km(p.lat, p.lng, q.lat, q.lng) < SAME_PLACE_KM)
            })
        });
        match home {
            Some(g) => {
                if !g.contains(&i) {
                    g.push(i);
                }
            }
            None => groups.push(vec![i]),
        }
    }
    groups
}

fn km(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lng2 - lng1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

/// `wof:population` (else `gn:population`) from the record's GeoJSON
/// properties; 0 when WoF has no population for it.
fn population(
    conns: &[Connection],
    place: &Place,
    cache: &mut HashMap<i64, i64>,
) -> Result<i64, String> {
    if let Some(&pop) = cache.get(&place.id) {
        return Ok(pop);
    }
    let body: Option<String> = conns[place.db]
        .query_row(
            "SELECT body FROM geojson WHERE id = ?1 AND is_alt = 0 LIMIT 1",
            [place.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("geojson {}: {e}", place.id))?;
    let pop = match body {
        Some(body) => {
            let gj: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| format!("geojson {}: {e}", place.id))?;
            ["wof:population", "gn:population"]
                .iter()
                .find_map(|k| gj.get("properties")?.get(*k)?.as_i64())
                .unwrap_or(0)
        }
        None => 0,
    };
    cache.insert(place.id, pop);
    Ok(pop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(alias: &str, names: &[&str]) -> Result<Rule, Reject> {
        classify(&tidy(alias), names)
    }

    #[test]
    fn tidy_joins_dotted_initials() {
        assert_eq!(tidy("N.Y.C."), "NYC");
        assert_eq!(tidy("S.F."), "SF");
        assert_eq!(tidy("S. Fran"), "S Fran");
        assert_eq!(tidy("  San   Fran "), "San Fran");
    }

    #[test]
    fn keeps_initialisms() {
        assert_eq!(rule("NYC", &["New York"]), Ok(Rule::Initialism));
        assert_eq!(rule("N.Y.C.", &["New York"]), Ok(Rule::Initialism));
        assert_eq!(rule("SF", &["San Francisco"]), Ok(Rule::Initialism));
        assert_eq!(rule("L.A.", &["Los Angeles"]), Ok(Rule::Initialism));
        assert_eq!(rule("Sf", &["San Francisco"]), Err(Reject::NoRule));
    }

    #[test]
    fn keeps_compact_and_abbreviated_names() {
        assert_eq!(rule("Newyork", &["New York"]), Ok(Rule::Compact));
        assert_eq!(rule("Kingslynn", &["King's Lynn"]), Ok(Rule::Compact));
        assert_eq!(
            rule("Henley-on-Thames", &["Henley on Thames"]),
            Err(Reject::NoRule)
        );
        assert_eq!(
            rule("NY City", &["New York", "New York City"]),
            Ok(Rule::Abbreviated)
        );
        assert_eq!(rule("San Fran", &["San Francisco"]), Ok(Rule::Abbreviated));
        assert_eq!(rule("S Fran", &["San Francisco"]), Err(Reject::NoRule));
        assert_eq!(rule("NY Cty", &["New York City"]), Err(Reject::NoRule));
        assert_eq!(
            rule("College Sta", &["College Station"]),
            Ok(Rule::Abbreviated)
        );
        assert_eq!(
            rule("Bayside Garden", &["Bayside Gardens"]),
            Err(Reject::NoRule)
        );
    }

    #[test]
    fn keeps_short_forms() {
        assert_eq!(rule("Philly", &["Philadelphia"]), Ok(Rule::Shortened));
        assert_eq!(rule("Phila", &["Philadelphia"]), Err(Reject::NoRule));
        assert_eq!(rule("Park", &["Parkville"]), Err(Reject::NoRule));
        assert_eq!(rule("Whites", &["White Plains"]), Err(Reject::NoRule));
        assert_eq!(rule("Brissie", &["Brisbane City"]), Ok(Rule::Shortened));
        assert_eq!(rule("Welly", &["Wellington"]), Ok(Rule::Shortened));
        assert_eq!(rule("Philthy", &["Philadelphia"]), Err(Reject::NoRule));
        assert_eq!(
            rule("Stratford", &["Stratford-upon-Avon"]),
            Err(Reject::NoRule)
        );
        assert_eq!(rule("Syd", &["Sydney"]), Err(Reject::NoRule));
    }

    #[test]
    fn drops_nicknames_and_codes() {
        assert_eq!(
            rule("The City", &["San Francisco"]),
            Err(Reject::LeadingThe)
        );
        assert_eq!(rule("Fog City", &["San Francisco"]), Err(Reject::NoRule));
        assert_eq!(rule("Bay Area", &["San Francisco"]), Err(Reject::NoRule));
        assert_eq!(rule("LAX", &["Los Angeles"]), Err(Reject::Code));
        assert_eq!(rule("PHL", &["Philadelphia"]), Err(Reject::Code));
        assert_eq!(rule("SC", &["Los Angeles"]), Err(Reject::NoRule));
        assert_eq!(rule("Lotus", &["Los Angeles"]), Err(Reject::NoRule));
        assert_eq!(rule("Smell A", &["Los Angeles"]), Err(Reject::NoRule));
    }

    fn place(name: &str, lat: f64, lng: f64) -> Place {
        Place {
            db: 0,
            id: 0,
            name: name.to_owned(),
            country: "US".to_owned(),
            lat,
            lng,
            canonical: vec![name.to_owned()],
            candidates: Vec::new(),
        }
    }

    #[test]
    fn same_city_records_group_together() {
        let places = [
            place("Philadelphia", 39.99, -75.17),
            place("Philadelphia", 40.01, -75.15),
            place("Philadelphia", 32.77, -89.12),
        ];
        assert_eq!(
            same_place_groups(&places, &[0, 1, 2]),
            vec![vec![0, 1], vec![2]]
        );
    }
}
