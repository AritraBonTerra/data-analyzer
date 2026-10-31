# Writes the small spreadsheet fixtures used by crates/core/tests.
# Run: uv run --with openpyxl --with xlwt scripts/make_excel_fixtures.py
import datetime as dt
import pathlib

import openpyxl
import xlwt

out = pathlib.Path(__file__).resolve().parent.parent / "crates/core/tests/fixtures"
rows = [
    ("id", "name", "price", "when", "ok", "mixed"),
    (1, "apple", 1.5, dt.datetime(2024, 1, 2, 3, 4, 5), True, 10),
    (2, "banana", None, dt.datetime(2024, 2, 3, 0, 0, 0), False, "ten"),
    (3, "cherry", 3.25, dt.datetime(2024, 3, 4, 12, 30, 0), True, None),
]

wb = openpyxl.Workbook()
ws = wb.active
ws.title = "Fruit"
for row in rows:
    ws.append(row)
wb.create_sheet("Empty")
wb.save(out / "fruit.xlsx")

book = xlwt.Workbook()
sheet = book.add_sheet("Fruit")
date_style = xlwt.easyxf(num_format_str="YYYY-MM-DD HH:MM:SS")
for r, row in enumerate(rows):
    for c, value in enumerate(row):
        if value is None:
            continue
        if isinstance(value, dt.datetime):
            sheet.write(r, c, value, date_style)
        else:
            sheet.write(r, c, value)
book.save(out / "fruit.xls")
