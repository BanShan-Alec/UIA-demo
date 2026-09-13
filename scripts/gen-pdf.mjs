// Generates testpage/sample.pdf — a one-page PDF with selectable text lines,
// used to probe the Chromium built-in PDF viewer's UIA TextPattern support.
// Written by hand (no deps): builds the object table and a correct xref.

import { writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const content = [
  'BT /F1 22 Tf 72 700 Td (UIA PDF probe test page) Tj ET',
  'BT /F1 16 Tf 72 660 Td (Line 1: The quick brown fox jumps over the lazy dog.) Tj ET',
  'BT /F1 16 Tf 72 630 Td (Line 2: Pack my box with five dozen liquor jugs.) Tj ET',
  'BT /F1 16 Tf 72 600 Td (Line 3: Chinese content 中文内容测试行。) Tj ET'
].join('\n')

const objects = [
  '<< /Type /Catalog /Pages 2 0 R >>',
  '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
  '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
  `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
  '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>'
]

let pdf = '%PDF-1.4\n'
const offsets = []
objects.forEach((body, i) => {
  offsets.push(pdf.length)
  pdf += `${i + 1} 0 obj\n${body}\nendobj\n`
})
const xrefStart = pdf.length
pdf += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`
for (const off of offsets) pdf += `${String(off).padStart(10, '0')} 00000 n \n`
pdf += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xrefStart}\n%%EOF`

const out = join(dirname(fileURLToPath(import.meta.url)), '..', 'testpage', 'sample.pdf')
writeFileSync(out, pdf, 'latin1')
console.log(`wrote ${out} (${pdf.length} bytes)`)
