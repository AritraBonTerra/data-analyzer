# Writes the awkward-but-realistic data files in crates/core/tests/fixtures/messy.
# Each one has a layout or value style that people really send around: titles
# above the table, totals rows, two tables in a sheet, "$1,234.00", and so on.
# Run: uv run --with openpyxl --with xlwt --with duckdb scripts/make_messy_fixtures.py
import datetime as dt
import json
import pathlib
import random

import duckdb
import openpyxl
import xlwt
from openpyxl.styles import Alignment, Font

out = (
    pathlib.Path(__file__).resolve().parent.parent / "crates/core/tests/fixtures/messy"
)
out.mkdir(parents=True, exist_ok=True)
rng = random.Random(7)

REGIONS = ["North", "South", "East", "West"]
PRODUCTS = [
    "Oat Milk 1L",
    "Cold Brew 330ml",
    "Granola 500g",
    "Protein Bar",
    "Kombucha 400ml",
]
MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun"]


def sales_rows(n):
    """Order lines: (order_id, date, region, product, units, unit_price, revenue)."""
    rows = []
    for i in range(n):
        day = dt.date(2026, 1, 1) + dt.timedelta(days=rng.randrange(180))
        units = rng.randint(1, 40)
        price = round(rng.choice([2.49, 3.99, 5.5, 1.75, 4.25]), 2)
        rows.append(
            (
                1001 + i,
                day,
                rng.choice(REGIONS),
                rng.choice(PRODUCTS),
                units,
                price,
                round(units * price, 2),
            )
        )
    return rows


def write(name, text, encoding="utf-8"):
    (out / name).write_bytes(text.encode(encoding))


rows = sales_rows(40)

# --- CSV ---------------------------------------------------------------

# 00: the well-behaved baseline.
lines = ["order_id,order_date,region,product,units,unit_price,revenue"]
lines += [",".join(str(v) for v in r) for r in sales_rows(500)]
write("00_clean_sales.csv", "\n".join(lines) + "\n")

# 01: a report export. Title block on top, totals and a footnote below.
lines = [
    "Northwind Foods - Weekly Sales Report",
    "Generated: 2026-07-03 09:14 by reporting@northwind.example",
    "Filters: Region = All; Channel = Retail",
    "",
    "Order ID,Order Date,Region,Product,Units,Unit Price,Revenue",
]
lines += [",".join(str(v) for v in r) for r in rows]
lines += [
    "",
    f"Total,,,,{sum(r[4] for r in rows)},,{sum(r[6] for r in rows):.2f}",
    "Note: revenue excludes returns processed after the reporting cutoff.",
]
write("01_report_preamble.csv", "\n".join(lines) + "\n")

# 02: European export. Semicolons, decimal commas, dd.mm.yyyy dates.
lines = ["Bestellnr;Datum;Region;Produkt;Menge;Einzelpreis;Umsatz"]
for r in rows:
    revenue = f"{r[6] * 37:,.2f}".replace(",", "X").replace(".", ",").replace("X", ".")
    price = f"{r[5]:.2f}".replace(".", ",")
    lines.append(f"{r[0]};{r[1]:%d.%m.%Y};{r[2]};{r[3]};{r[4]};{price};{revenue}")
write("02_semicolon_european.csv", "\n".join(lines) + "\n")

# 03: numbers a human formatted. Currency, thousands separators, percents,
# accounting negatives and several spellings of "missing".
lines = ["account,region,revenue,cost,margin_pct,yoy_growth,headcount"]
missing = ["N/A", "-", "n/a", "", "TBD"]
for i in range(30):
    revenue = rng.randint(8_000, 2_400_000)
    cost = int(revenue * rng.uniform(0.4, 0.9))
    growth = rng.uniform(-0.3, 0.6)
    growth_text = (
        f"({abs(growth) * 100:.1f}%)" if growth < 0 else f"{growth * 100:.1f}%"
    )
    head = rng.choice(missing) if i % 6 == 0 else str(rng.randint(3, 220))
    lines.append(
        f'ACC-{i:03d},{rng.choice(REGIONS)},"${revenue:,.2f}","${cost:,}",{(revenue - cost) / revenue * 100:.1f}%,{growth_text},{head}'
    )
write("03_currency_percent.csv", "\n".join(lines) + "\n")

# 04: two tables stacked in one file with a blank line between them.
lines = ["Region,Orders,Revenue"]
lines += [f"{r},{rng.randint(50, 400)},{rng.randint(5000, 90000)}" for r in REGIONS]
lines += ["", "Product,SKU,Units Sold,Returns,Return Rate"]
lines += [
    f"{p},SKU-{i:04d},{rng.randint(200, 5000)},{rng.randint(0, 90)},{rng.uniform(0, 4):.2f}%"
    for i, p in enumerate(PRODUCTS)
]
write("04_two_tables.csv", "\n".join(lines) + "\n")

# 05: no header row at all.
write("05_no_header.csv", "\n".join(",".join(str(v) for v in r) for r in rows) + "\n")

# 06: two header rows (group, then field) and a units row under them.
lines = [
    ",,Sales,Sales,Inventory,Inventory",
    "Store,Week,Units,Revenue,On Hand,On Order",
    ",,(each),(USD),(each),(each)",
]
for s in range(1, 6):
    for w in range(1, 5):
        lines.append(
            f"Store {s},2026-W{w:02d},{rng.randint(100, 900)},{rng.randint(900, 9000)},{rng.randint(0, 500)},{rng.randint(0, 300)}"
        )
write("06_multirow_header.csv", "\n".join(lines) + "\n")

# 07: wide layout, one column per month. Fine to read, awkward to chart.
lines = ["Region,Product," + ",".join(f"{m} 2026" for m in MONTHS)]
for region in REGIONS:
    for product in PRODUCTS[:3]:
        lines.append(
            f"{region},{product},"
            + ",".join(str(rng.randint(200, 3000)) for _ in MONTHS)
        )
write("07_wide_months.csv", "\n".join(lines) + "\n")

# 08: Latin-1 (Windows-1252) text, as old Excel "Save as CSV" writes it.
lines = ["kunde,stadt,betrag"]
people = [
    ("Jürgen Müller", "München"),
    ("Zoë Brontë", "Besançon"),
    ("José Peña", "A Coruña"),
    ("Søren Åberg", "Malmö"),
]
lines += [
    f"{n},{c},{rng.randint(10, 900)}.{rng.randint(0, 99):02d}" for n, c in people * 3
]
write("08_latin1.csv", "\n".join(lines) + "\n", encoding="cp1252")

# 09: BOM, padded and duplicate header names, and a trailing comma on every row.
lines = ["﻿id, name ,amount,amount,notes,"]
lines += [
    f"{i}, item {i} ,{rng.randint(1, 99)},{rng.randint(1, 99)},," for i in range(1, 13)
]
write("09_bom_trailing_delims.csv", "\n".join(lines) + "\n")

# 10: identifiers that look like numbers, mixed date spellings, Yes/No flags.
lines = ["zip,phone,sku,signup,active,last_seen"]
zips = ["02134", "00501", "94107", "10001", "07302"]
for i in range(24):
    d = dt.date(2025, 1, 1) + dt.timedelta(days=rng.randrange(500))
    signup = [f"{d:%m/%d/%Y}", f'"{d:%b} {d.day}, {d.year}"', f"{d:%Y-%m-%d}"][i % 3]
    seen = f"{d + dt.timedelta(days=30):%d/%m/%Y}"
    lines.append(
        f"{rng.choice(zips)},0{rng.randint(7000000000, 7999999999)},000{rng.randint(100, 999)},{signup},{rng.choice(['Yes', 'No', 'Y', 'N'])},{seen}"
    )
write("10_ids_dates_flags.csv", "\n".join(lines) + "\n")

# 11: instrument-style export. "key: value" metadata, then a tab-separated table.
lines = [
    "# Device: ThermoLog TL-200",
    "# Serial: TL2-88213",
    "# Started: 2026-05-02T08:00:00Z",
    "# Interval: 60 s",
    "",
    "timestamp\ttemp_c\thumidity\tbattery_v",
]
t = dt.datetime(2026, 5, 2, 8)
for i in range(60):
    lines.append(
        f"{t + dt.timedelta(minutes=i):%Y-%m-%d %H:%M:%S}\t{20 + rng.uniform(-2, 4):.2f}\t{45 + rng.uniform(-5, 10):.1f}\t{3.7 - i * 0.002:.3f}"
    )
write("11_metadata_then_table.txt", "\n".join(lines) + "\n")

# 12: subtotal rows mixed into the data, and a grand total at the end.
lines = ["Region,Product,Units,Revenue"]
grand_u = grand_r = 0
for region in REGIONS:
    sub_u = sub_r = 0
    for product in PRODUCTS[:3]:
        u, r = rng.randint(100, 900), rng.randint(900, 9000)
        sub_u, sub_r = sub_u + u, sub_r + r
        lines.append(f"{region},{product},{u},{r}")
    lines.append(f"{region} Total,,{sub_u},{sub_r}")
    grand_u, grand_r = grand_u + sub_u, grand_r + sub_r
lines.append(f"Grand Total,,{grand_u},{grand_r}")
write("12_subtotals.csv", "\n".join(lines) + "\n")

# --- Excel -------------------------------------------------------------

bold = Font(bold=True)

# 20: the classic report sheet. Merged title, subtitle, a gap, the table
# indented one column, a totals row, then a footnote.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Weekly Sales"
ws["A1"] = "Northwind Foods - Weekly Sales Report"
ws["A1"].font = Font(bold=True, size=14)
ws.merge_cells("A1:H1")
ws["A2"] = "Generated 3 Jul 2026 · Region: All · Channel: Retail"
ws.merge_cells("A2:H2")
header = [
    "Order ID",
    "Order Date",
    "Region",
    "Product",
    "Units",
    "Unit Price",
    "Revenue",
]
for c, name in enumerate(header, start=2):
    ws.cell(row=5, column=c, value=name).font = bold
for r, row in enumerate(rows, start=6):
    for c, value in enumerate(row, start=2):
        ws.cell(row=r, column=c, value=value)
last = 6 + len(rows)
ws.cell(row=last + 1, column=2, value="Total").font = bold
ws.cell(row=last + 1, column=6, value=sum(r[4] for r in rows))
ws.cell(row=last + 1, column=8, value=round(sum(r[6] for r in rows), 2))
ws.cell(
    row=last + 3,
    column=2,
    value="Note: revenue excludes returns processed after the reporting cutoff.",
)
wb.save(out / "20_report_title.xlsx")

# 21: two-level header with merged group cells.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Quarterly"
ws.append(["Region", "Q1", None, "Q2", None])
ws.merge_cells("B1:C1")
ws.merge_cells("D1:E1")
ws.merge_cells("A1:A2")
ws.append([None, "Units", "Revenue", "Units", "Revenue"])
for cell in ws[1] + ws[2]:
    cell.font = bold
    cell.alignment = Alignment(horizontal="center")
for region in REGIONS:
    ws.append(
        [
            region,
            rng.randint(900, 5000),
            rng.randint(9000, 90000),
            rng.randint(900, 5000),
            rng.randint(9000, 90000),
        ]
    )
wb.save(out / "21_merged_headers.xlsx")

# 22: two tables side by side on one sheet with a gap column between them.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Dashboard"
ws["A1"] = "Revenue by region"
ws["F1"] = "Top products"
for c, name in enumerate(["Region", "Orders", "Revenue"], start=1):
    ws.cell(row=2, column=c, value=name).font = bold
for c, name in enumerate(["Product", "Units", "Return rate"], start=6):
    ws.cell(row=2, column=c, value=name).font = bold
for r, region in enumerate(REGIONS, start=3):
    ws.cell(row=r, column=1, value=region)
    ws.cell(row=r, column=2, value=rng.randint(50, 400))
    ws.cell(row=r, column=3, value=rng.randint(5000, 90000))
for r, product in enumerate(PRODUCTS, start=3):
    ws.cell(row=r, column=6, value=product)
    ws.cell(row=r, column=7, value=rng.randint(200, 5000))
    cell = ws.cell(row=r, column=8, value=round(rng.uniform(0, 0.04), 4))
    cell.number_format = "0.0%"
wb.save(out / "22_side_by_side_tables.xlsx")

# 23: a crosstab whose header cells are numbers (years), plus numbers stored
# as text, percent and currency formats, and formula errors.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "By Year"
ws.append(["Region", 2022, 2023, 2024, 2025])
for region in REGIONS:
    ws.append([region] + [rng.randint(100_000, 900_000) for _ in range(4)])
ws2 = wb.create_sheet("Formats")
ws2.append(["account", "revenue_text", "revenue_num", "margin", "ratio", "closed_on"])
for i in range(12):
    revenue = rng.randint(8_000, 900_000)
    ws2.append(
        [
            f"ACC-{i:03d}",
            f"{revenue:,}",
            revenue,
            round(rng.uniform(0.05, 0.6), 3),
            "#DIV/0!" if i % 5 == 0 else round(rng.uniform(0.5, 3), 2),
            f"{rng.randint(1, 28):02d}/{rng.randint(1, 12):02d}/2026",
        ]
    )
    row = ws2.max_row
    ws2.cell(row=row, column=3).number_format = '"$"#,##0'
    ws2.cell(row=row, column=4).number_format = "0.0%"
wb.save(out / "23_year_headers_and_formats.xlsx")

# 24: a workbook whose first sheet is a cover page, with the data on sheet 3.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "README"
for line in [
    "Northwind Foods data extract",
    "",
    "Prepared by: Finance Ops",
    "Contact: finance@northwind.example",
    "",
    "Sheets:",
    "  Definitions - what each column means",
    "  Orders - one row per order line",
]:
    ws.append([line])
defs = wb.create_sheet("Definitions")
defs.append(["Column", "Meaning"])
for name, meaning in [
    ("order_id", "Unique order line"),
    ("units", "Units shipped"),
    ("revenue", "Units x unit price, USD"),
]:
    defs.append([name, meaning])
orders = wb.create_sheet("Orders")
orders.append(
    ["order_id", "order_date", "region", "product", "units", "unit_price", "revenue"]
)
for r in sales_rows(200):
    orders.append(list(r))
wb.save(out / "24_cover_sheet_first.xlsx")

# 25: the report layout again, as legacy .xls.
book = xlwt.Workbook()
sheet = book.add_sheet("Weekly Sales")
sheet.write(0, 0, "Northwind Foods - Weekly Sales Report")
sheet.write(1, 0, "Generated 3 Jul 2026")
date_style = xlwt.easyxf(num_format_str="YYYY-MM-DD")
for c, name in enumerate(header):
    sheet.write(3, c, name)
for r, row in enumerate(rows, start=4):
    for c, value in enumerate(row):
        if isinstance(value, dt.date):
            sheet.write(r, c, value, date_style)
        else:
            sheet.write(r, c, value)
sheet.write(4 + len(rows), 0, "Total")
sheet.write(4 + len(rows), 6, round(sum(r[6] for r in rows), 2))
book.save(out / "25_report_title.xls")

# 26-28 use fixed values, so the files after them stay as they were.
# 26: a plain table at A1 with an empty spacer column after the first.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Report"
for row in [
    ["Region", None, "Units", "Revenue", "Margin"],
    ["North", None, 10, 100.5, 0.2],
    ["South", None, 20, 200.25, 0.3],
    ["East", None, 5, 50, 0.1],
]:
    ws.append(row)
wb.save(out / "26_spacer_column.xlsx")

# 27: one sheet per month, each with the same title right above its table.
wb = openpyxl.Workbook()
for i, month in enumerate(["Jan", "Feb", "Mar"]):
    ws = wb.active if i == 0 else wb.create_sheet()
    ws.title = month
    ws.append(["Monthly Sales Report"])
    ws.append(["Region", "Units", "Revenue"])
    for row in [["North", 10, 100.5], ["South", 20, 200.25], ["East", 5, 50]]:
        ws.append(row)
wb.save(out / "27_monthly_sheets.xlsx")

# 28: a title right above the header and a one-cell footer right below.
wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Follow ups"
ws.append(["Customer follow-ups"])
ws.append(["id", "customer", "amount", "Notes"])
for row in [
    [1, "Ada", 120.5, "Called twice, no answer."],
    [2, "Bob", 80, None],
    [3, "Cy", 45.25, "Asked for a refund."],
    [4, "Di", 300, "Total satisfaction reported"],
]:
    ws.append(row)
ws.append(["Prepared by support team"])
wb.save(out / "28_title_and_footer.xlsx")

# --- JSON --------------------------------------------------------------

records = [
    {
        "id": r[0],
        "date": r[1].isoformat(),
        "customer": {
            "name": f"Customer {i}",
            "tier": rng.choice(["gold", "silver"]),
            "address": {
                "city": rng.choice(["Leeds", "Austin", "Pune"]),
                "zip": "02134",
            },
        },
        "items": [
            {"sku": f"SKU-{rng.randint(1, 9)}", "qty": rng.randint(1, 5)}
            for _ in range(rng.randint(1, 3))
        ],
        "total": r[6],
    }
    for i, r in enumerate(rows)
]

# 30: an API response. The rows live under "data", with paging info beside them.
write(
    "30_api_envelope.json",
    json.dumps(
        {"status": "ok", "page": 1, "total_pages": 4, "data": records}, indent=2
    ),
)

# 31: nested objects and arrays, one record per line.
write("31_nested.jsonl", "\n".join(json.dumps(r) for r in records) + "\n")

# 32: column names in one array, rows as arrays in another.
write(
    "32_columns_and_rows.json",
    json.dumps(
        {
            "columns": ["order_id", "region", "units", "revenue"],
            "rows": [[r[0], r[2], r[4], r[6]] for r in rows],
        }
    ),
)

# --- Parquet -----------------------------------------------------------

# 40: nested and exotic types (struct, list, decimal, timestamptz).
duckdb.sql(
    f"""
    COPY (
        SELECT i AS id,
               {{'name': 'Customer ' || i, 'tier': CASE WHEN i % 3 = 0 THEN 'gold' ELSE 'silver' END}} AS customer,
               [i % 5, i % 7, i % 11] AS scores,
               CAST(i * 12.345 AS DECIMAL(18, 3)) AS amount,
               TIMESTAMPTZ '2026-01-01 00:00:00+00' + INTERVAL (i) HOUR AS seen_at,
               MAP {{'k': i}} AS attrs
        FROM range(200) t(i)
    ) TO '{out / "40_nested.parquet"}' (FORMAT parquet)
    """
)

print("\n".join(sorted(p.name for p in out.iterdir())))
