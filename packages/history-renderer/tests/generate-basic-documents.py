"""Generate the four producer fixtures into a caller-supplied test directory."""

import sys
from pathlib import Path

from docx import Document
from openpyxl import Workbook
from pptx import Presentation
from pypdf import PdfWriter

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)

document = Document()
document.add_paragraph("base")
document.add_paragraph("second")
document.save(out / "plain.docx")

book = Workbook()
book.active.append(["base", 2])
book.save(out / "plain.xlsx")

slides = Presentation()
for _ in range(2):
    slides.slides.add_slide(slides.slide_layouts[6])
slides.save(out / "plain.pptx")

pdf = PdfWriter()
pdf.add_blank_page(width=200, height=200)
pdf.add_metadata({"/Title": "Base"})
pdf.write(out / "plain.pdf")
