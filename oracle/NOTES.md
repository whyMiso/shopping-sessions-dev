# Oracle notes

Written from `rules.md` alone (plus fixtures). Single-threaded Rust.

Build: `cargo build --release` -> `target/release/oracle [DATA OUT]`
(DATA_DIR / OUTPUT_DIR env vars, defaults /data and /output; the last two CLI
args override them when given).

## Readings of the rules (where a choice was made)

- **Header**: the first line of every CSV file is skipped unconditionally, even
  if it is not the header text. A later line equal to the header is data
  (fixture L3 relies on this).
- **Lines**: split on `\n` only. A final line without `\n` is a line. Bytes
  after the last `\n` that are empty are not a line (no "empty last line").
- **Quotes**: a field is quoted iff its first byte is `"`; it must then be at
  least 2 bytes, end with `"`, and contain no other `"`. A `"` anywhere else in
  a field makes the line malformed. `""` is an empty field (= null).
- **products.csv / campaigns.csv** go through the same CSV parser; malformed
  lines there are skipped (they are promised clean). On a duplicate key the
  first line wins.
- **ingest_ts** is promised to be valid F1. The oracle parses it with the
  general timestamp parser (F1/F2/F3); if it is unparsable (or an F3 too big
  for i64) the line is treated as malformed and dropped in step 1. This never
  happens on generator data.
- **Event dedup tie** (equal `ingest_ts`): the first copy read is kept (rules
  say such copies are identical). Files are read in sorted path order.
- **Click dedup**: the first copy that parses (non-empty click_id) is kept;
  validation happens on that copy only (rules say copies are identical).
- **F3**: digits only, at least one digit, any number of leading zeros. A value
  that overflows i64 is valid-but-outside-every-window. `0` is 1970-01-01.
- **F1/F2 year**: any 4 ASCII digits (0000-9999), proleptic Gregorian;
  out-of-window years are simply out of window.
- **Sessions**: events of a person are sorted by `ts`; a new session starts when
  `ts - previous ts > 1,800,000`. Equal timestamps have gap 0, so they always
  share a session.
- **Price**: for purchases only; digits only (ASCII), leading zeros allowed,
  value 1..=10,000,000. View/cart prices are ignored entirely.
- **top_category**: computed over the summed purchase prices of all sessions of
  that (person, day); ties -> byte-wise smallest category name. `unknown`
  competes like any category.
- **Non-UTF-8 bytes** are carried as bytes throughout; only when writing the
  Parquet string columns are they converted with lossy UTF-8 (never on
  generator data).
