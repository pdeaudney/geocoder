-- Bootstrap SQL for DuckDB inspection sessions. Drop views for every
-- CSV the index-dumper emits, with explicit quote character so embedded
-- commas in street/place names don't trip the strict parser.
--
-- Invoke inside the dump dir:
--   cd data/index/dump-csv
--   duckdb -init /path/to/this/bootstrap.sql
-- Or use the dot-command inside an interactive session:
--   .read /path/to/this/bootstrap.sql

CREATE OR REPLACE VIEW streets AS
  SELECT * FROM read_csv_auto('streets.csv', quote='"');

CREATE OR REPLACE VIEW place_points AS
  SELECT * FROM read_csv_auto('place_points.csv', quote='"');

CREATE OR REPLACE VIEW admin_polygons AS
  SELECT * FROM read_csv_auto('admin_polygons.csv', quote='"');

CREATE OR REPLACE VIEW addr_points AS
  SELECT * FROM read_csv_auto('addr_points.csv', quote='"');

CREATE OR REPLACE VIEW i18n_names AS
  SELECT * FROM read_csv_auto('i18n_names.csv', quote='"');

.print 'Loaded views: streets, place_points, admin_polygons, addr_points, i18n_names'
