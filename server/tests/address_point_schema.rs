use query_server::address_points::AddressPointIndex;
use query_server::{
    Index, DEFAULT_ADMIN_CELL_LEVEL, DEFAULT_SEARCH_DISTANCE, DEFAULT_STREET_CELL_LEVEL,
};
use std::fs;
use std::io::Read;

#[test]
fn old_address_point_files_are_rejected_before_casting() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("gnaf_points.bin");
    let cells = dir.path().join("gnaf_cells.bin");
    let entries = dir.path().join("gnaf_entries.bin");
    let strings = dir.path().join("gnaf_strings.bin");

    fs::write(&points, [0u8; 24]).unwrap(); // old record layout
    fs::write(&cells, [0u8; 12]).unwrap();
    fs::write(&entries, [0u8; 2]).unwrap();
    fs::write(&strings, [0u8]).unwrap();

    let err = AddressPointIndex::open(&points, &cells, &entries, &strings)
        .err()
        .expect("legacy index must fail instead of silently losing records");
    assert!(err.contains("schema"), "{err}");
}

#[test]
fn address_point_schema_matches_layout_and_file_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("oa_us_points.bin");
    let cells = dir.path().join("oa_us_cells.bin");
    let entries = dir.path().join("oa_us_entries.bin");
    let strings = dir.path().join("oa_us_strings.bin");
    fs::write(&points, [0u8; 28]).unwrap();
    fs::write(&cells, [0u8; 12]).unwrap();
    fs::write(&entries, [0u8; 2]).unwrap();
    fs::write(&strings, [0u8]).unwrap();
    AddressPointIndex::write_schema(&points, &cells, &entries, &strings).unwrap();
    assert!(AddressPointIndex::open(&points, &cells, &entries, &strings)
        .unwrap()
        .is_some());

    let schema_path = dir.path().join("oa_us_points.schema.json");
    let mut schema: serde_json::Value =
        serde_json::from_slice(&fs::read(&schema_path).unwrap()).unwrap();
    schema["record"]["fields"][6][2] = 0.into();
    fs::write(&schema_path, serde_json::to_vec(&schema).unwrap()).unwrap();
    assert!(AddressPointIndex::open(&points, &cells, &entries, &strings).is_err());

    fs::write(&cells, [0u8; 13]).unwrap();
    AddressPointIndex::write_schema(&points, &cells, &entries, &strings).unwrap();
    assert!(AddressPointIndex::open(&points, &cells, &entries, &strings).is_err());
}

#[test]
fn legacy_reverse_manifest_is_rejected_before_mapping_records() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("manifest_reverse.json"),
        r#"{"tool":"reverse"}"#,
    )
    .unwrap();
    let err = Index::load(
        dir.path().to_str().unwrap(),
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .err()
    .expect("legacy reverse index must fail before reading record bytes");
    assert!(err.contains("schema"), "{err}");
}

#[test]
fn loaded_reverse_index_reserves_zero_for_missing_strings() {
    let Ok(dir) = std::env::var("GEOCODER_INDEX_DIR") else {
        return;
    };
    let index = Index::load(
        &dir,
        DEFAULT_STREET_CELL_LEVEL,
        DEFAULT_ADMIN_CELL_LEVEL,
        DEFAULT_SEARCH_DISTANCE,
    )
    .expect("load reverse index");
    assert_eq!(index.get_string(0), "");
    let mut first = [1u8; 1];
    fs::File::open(format!("{dir}/strings.bin"))
        .unwrap()
        .read_exact(&mut first)
        .unwrap();
    assert_eq!(first[0], 0);
}
