#!/usr/bin/env python3
import hashlib
import json
import sys
import zipfile
from pathlib import Path
from xml.sax.saxutils import escape

W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'


def paragraph(text, font='Calibri', east='Malgun Gothic'):
    return (f'<w:p><w:r><w:rPr><w:rFonts w:ascii="{font}" w:hAnsi="{font}" '
            f'w:eastAsia="{east}"/><w:sz w:val="22"/></w:rPr>'
            f'<w:t xml:space="preserve">{escape(text)}</w:t></w:r></w:p>')


parts = [
    paragraph('SYSTEM-WEBVIEW-FIRST 제목: Korean 한글 / Latin punctuation, (가나다)!'),
    paragraph('SYSTEM-WEBVIEW-GO Mixed wrapping: 가나다 abc, punctuation (한글)!', 'SeuOffice Sans GO', 'SeuOffice Sans GO'),
    paragraph('SYSTEM-WEBVIEW-KR 한국어와 English, 문장 부호 (wrap)!', 'SeuOffice Sans KR', 'SeuOffice Sans KR'),
    paragraph('SYSTEM-WEBVIEW-FALLBACK 실제 앱 기본 대체 글꼴: Calibri와 맑은 고딕.'),
]
rows = []
for a, b in [('SYSTEM-WEBVIEW-TABLE', '표와 table'), ('둘째 행', 'Second row')]:
    rows.append('<w:tr>' + ''.join('<w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr>' + paragraph(cell) + '</w:tc>' for cell in (a, b)) + '</w:tr>')
parts.append('<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/></w:tblPr>' + ''.join(rows) + '</w:tbl>')
for i in range(1, 85):
    parts.append(paragraph(f'SYSTEM-WEBVIEW-BODY-{i:03d} 문서 진단 문장입니다. Mixed Latin and 한글, punctuation (가나다): page boundary and wrapping. ' * 2))
parts.append(paragraph('SYSTEM-WEBVIEW-LAST 마지막 문단.'))
parts.append('<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr>')
document = f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W}"><w:body>{"".join(parts)}</w:body></w:document>'
styles = f'''<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W}"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:eastAsia="Malgun Gothic"/><w:sz w:val="22"/><w:lang w:val="ko-KR" w:eastAsia="ko-KR"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style></w:styles>'''
types = '''<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>'''
rels = '''<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>'''
doc_rels = '''<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>'''
output = Path(sys.argv[1])
with zipfile.ZipFile(output, 'w', compression=zipfile.ZIP_STORED) as archive:
    for name, content in [('[Content_Types].xml', types), ('_rels/.rels', rels), ('word/_rels/document.xml.rels', doc_rels), ('word/document.xml', document), ('word/styles.xml', styles)]:
        info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_STORED
        archive.writestr(info, content.encode('utf-8'))
print(json.dumps({'file': output.name, 'sha256': hashlib.sha256(output.read_bytes()).hexdigest(), 'markers': ['SYSTEM-WEBVIEW-FIRST', 'SYSTEM-WEBVIEW-GO', 'SYSTEM-WEBVIEW-KR', 'SYSTEM-WEBVIEW-FALLBACK', 'SYSTEM-WEBVIEW-TABLE', 'SYSTEM-WEBVIEW-LAST']}))
