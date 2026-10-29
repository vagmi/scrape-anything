# Skill: analyzing files in the sandbox

When the user attaches files (CSV, JSON, JSONL, anything), they land in your
**session workspace**, and `run_command` gives you a Linux shell next to them.
Parse and analyze with scripts there. **Don't** `read_file` a whole data file
into your context: it's slow, expensive, and you'll miscount rows.

## Where things are

| In the file tools | Inside `run_command` | What |
|---|---|---|
| `work/uploads/…` | `/work/uploads/…` | The user's files. **Read-only.** |
| `work/out/…` | `/work/out/…` | Your outputs. Write results here. |
| `work/session/artifact.json` | `/work/session/artifact.json` | Rows this session has collected so far (refreshed before every command). |
| `work/<anything>` | `/work/<anything>` | Scratch space, e.g. your scripts. |

The workspace belongs to this session. To reuse a script on later runs for this
site, also `write_file` it to your per-site memory (a path without `work/`).

## The sandbox

- `run_command({ command, timeout_secs? })` runs `bash -c` in `/work`. Default
  timeout is 30s (the server caps it); returns
  `{ exit_code, stdout, stderr, timed_out, truncated, duration_ms }`.
- Installed: `python3` with **pandas** 3 and **duckdb** 2, **bun**, **jq**,
  `curl`. The network is on, so `pip install --user x` / `bun add x` work if
  you really need more. Installs last as long as the session's container.
- stdout/stderr are clipped (head and tail kept). Print **summaries**, such as
  counts, `df.head()` or `describe()`, and write full results to `/work/out/`.
- Each call is a fresh shell. Variables don't persist between calls, but files do.

## Workflow

1. **Look at the preview first.** The attachment note gives each file's size
   and shape (CSV header + first rows, JSON type/keys/length). Often that's
   enough to write the parser straight away.
2. **Probe cheaply** if you need more: `head -5`, `wc -l`, `jq 'keys'`,
   `jq '.[0]'`, `jq 'length'`.
3. **Write the script with `write_file`** (`work/parse.py`), then run it with
   `run_command({ command: "python3 parse.py" })`. Edit with `edit_file` and
   rerun when it fails. That's easier than shell quoting a long `python -c`.
4. **Save the result**: write rows as JSON to `/work/out/`, then
   `save_result({ path: "work/out/rows.json" })`. That becomes the session's
   downloadable JSON/CSV, exactly like an `execute_script` extraction
   (`append: true` merges onto rows already collected).
5. Finish with a short summary: rows in, rows out, what you did.

## Picking a tool

- **jq**: quick looks at JSON and simple reshapes.
  `jq -c '.items[] | {name, url}' uploads/x.json > out/rows.jsonl`
- **duckdb**: SQL directly over CSV/JSON/Parquet files, fast on large files,
  handles messy CSVs well (`read_csv_auto`). Good for filters, group-bys and joins.
- **pandas**: cleaning and reshaping row by row.
- **bun**: when JavaScript is more natural (e.g. reusing an extractor's logic).

### duckdb

```python
import duckdb
con = duckdb.connect()
rel = con.sql("""
    SELECT company, count(*) AS n
    FROM read_csv_auto('/work/uploads/leads.csv')
    GROUP BY company ORDER BY n DESC
""")
print(rel.limit(10))                     # pretty table on stdout
rel.write_csv('/work/out/by_company.csv')
rows = rel.fetchall()                    # list of tuples (there is no fetchone on a relation)
df = rel.df()                            # → pandas
```

Note the duckdb 2 API: a relation has `fetchall()`, `df()`, `to_dicts()`,
`write_csv()`, but no `fetchone()`. Use `con.execute(sql).fetchone()` if you
want a single row.

### pandas → result rows

```python
import pandas as pd
df = pd.read_csv('/work/uploads/leads.csv')
df = df.dropna(subset=['url']).drop_duplicates('url')
df.to_json('/work/out/rows.json', orient='records', force_ascii=False)
print(len(df), 'rows'); print(df.head())
```

## Uploads + the browser

A common job: the upload is a list of URLs or names, and the user wants each
one looked up on the web. Combine the two halves:

1. Extract the list in the sandbox (`print` a compact JSON array of the URLs,
   or write it to `/work/out/todo.json` and `read_file` that small file).
2. For each URL: `navigate`, then `execute_script` returning
   `{ kind, items, append: true }`, so rows accumulate in the session.
3. To join the scraped rows back onto the upload, read
   `/work/session/artifact.json` in a script, merge with pandas, write
   `/work/out/merged.json`, and `save_result` it (without `append`, so the
   merged table replaces the raw rows).

Bound the loop like any pagination: tell the user how many you covered.
