# Structured output contract

This crate adds channel-neutral table and chart blocks to the existing Markdown output protocol. Hosts advertise `tables` and `charts` independently as `disabled`, `text`, or `native`. The Feishu adapter projects validated data into Card 2.0 components; the TUI renders readable Markdown data tables.

```remi-table
{"title":"Results","columns":[{"id":"name","label":"Name"},{"id":"count","label":"Count","kind":"number"}],"rows":[{"name":"Alpha","count":7},{"name":"Beta","count":11}],"page_size":5}
```

```remi-chart
{"title":"Counts","kind":"bar","points":[{"label":"Alpha","value":7},{"label":"Beta","value":11}]}
```

- Tables accept 1-20 uniquely named columns and at most 100 rows. Column IDs use ASCII letters, digits, and underscores; prototype-related names are reserved. Column kinds are `text` (default) and `number`. Cells are scalar JSON values; missing/null cells are blank. Numeric columns reject strings. Pagination is 1-10 rows per page, default 5.
- Charts accept `bar`, `line`, and `pie`, with 1-200 points. Each point has a label and numeric value, plus an optional series for bar/line. Label/series pairs must be unique. Pie values are nonnegative and at least one is positive; pie charts do not accept series.
- Blocks are limited to 16 KiB of JSON. Unknown fields, arbitrary renderer specifications, callbacks, and executable formatters are rejected. Code examples inside an outer Markdown fence have no structured semantics.
- Text fallback retains every row/point. Feishu uses at most five native tables and five native charts per card, projecting additional blocks as Markdown. Oversized final cards use the existing bounded plain-text delivery fallback.
- Native table/chart data is extracted when quoting generated Feishu cards, so follow-up model turns can read the displayed values.

These are structured data displays. Table pagination and chart preview are renderer features; protocol blocks do not authorize external actions or tool execution.
