#!/usr/bin/env python3
"""Throwaway hosted-only engine diagnostic, not an app acceptance verifier."""
import argparse
import base64
import hashlib
import json
import shutil
import subprocess
import sys
import threading
import xml.etree.ElementTree as ET
import zipfile
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def run_driver(command, output):
    output.mkdir()
    with (output / 'stdout.log').open('wb') as stdout, (output / 'stderr.log').open('wb') as stderr:
        try:
            process = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        except OSError as error:
            return {'returncode': None, 'error': f'could not start owned diagnostic host: {error}'}
        try:
            code = process.wait(timeout=210)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            return {'returncode': process.returncode, 'error': 'owned diagnostic host exceeded 210 seconds'}
    result = {'returncode': code}
    try:
        result['native'] = json.loads((output / 'result.json').read_text())
    except (OSError, ValueError) as error:
        result['error'] = str(error)
    return result


def verify_saved(result, output, fixture):
    outcome = result['native']['outcome']
    saved = base64.b64decode(outcome['editSave']['savedBase64'], validate=True)
    if digest(saved) != outcome['editSave']['savedSha256']:
        raise ValueError('saved DOCX SHA does not match reported bytes')
    saved_path = output / 'saved.docx'
    saved_path.write_bytes(saved)
    with zipfile.ZipFile(saved_path) as archive:
        document = ET.fromstring(archive.read('word/document.xml'))
    text = ''.join(document.itertext())
    required = ['SYSTEM-WEBVIEW-FIRST', 'SYSTEM-WEBVIEW-GO', 'SYSTEM-WEBVIEW-KR',
                'SYSTEM-WEBVIEW-FALLBACK', 'SYSTEM-WEBVIEW-TABLE', 'SYSTEM-WEBVIEW-LAST',
                'SYSTEM-WEBVIEW-EDIT-20261116']
    missing = [marker for marker in required if marker not in text]
    if missing:
        raise ValueError(f'saved DOCX lost markers: {missing}')
    namespace = {'w': 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'}
    with zipfile.ZipFile(fixture) as archive:
        original = ET.fromstring(archive.read('word/document.xml'))
    paragraphs = lambda root: [''.join(node.text or '' for node in paragraph.findall('.//w:t', namespace))
                               for paragraph in root.findall('.//w:p', namespace)]
    expected = paragraphs(original)
    if not expected or expected[0].count('SYSTEM-WEBVIEW-FIRST') != 1:
        raise ValueError('fixture has no unique first edit target')
    expected[0] = expected[0].replace('SYSTEM-WEBVIEW-FIRST',
                                    'SYSTEM-WEBVIEW-FIRSTSYSTEM-WEBVIEW-EDIT-20261116', 1)
    if paragraphs(document) != expected:
        raise ValueError('saved DOCX changed ordered paragraph text beyond the one requested edit')
    original_tables = original.findall('.//w:tbl', namespace)
    saved_tables = document.findall('.//w:tbl', namespace)
    if len(saved_tables) != len(original_tables) or not original_tables:
        raise ValueError('saved DOCX did not retain its table')
    return {'sha256': digest(saved), 'bytes': len(saved), 'allMarkersRetained': True,
            'tables': len(saved_tables), 'scope': 'embed serialization, not native filesystem save'}


def inspect_pdf(path):
    import pypdfium2 as pdfium
    pdf = pdfium.PdfDocument(str(path))
    pages = []
    try:
        for index in range(len(pdf)):
            page = pdf[index]
            try:
                textpage = page.get_textpage()
                try:
                    text = textpage.get_text_range()
                finally:
                    textpage.close()
                bitmap = page.render(scale=96 / 72)
                try:
                    image = bitmap.to_pil().convert('RGBA')
                    image_path = path.parent / f'pdf-page-{index + 1:03d}.png'
                    image.save(image_path)
                    pages.append({'mediaBox': page.get_mediabox(fallback_ok=False),
                                  'pageSize': page.get_size(), 'pixelSize': image.size,
                                  'pixelSha256': digest(image.tobytes()),
                                  'image': image_path.name, 'textSha256': digest(text.encode('utf-8'))})
                    image.close()
                finally:
                    bitmap.close()
            finally:
                page.close()
    finally:
        pdf.close()
    if not pages:
        raise ValueError('PDF has no pages')
    return {'pages': len(pages), 'pageFacts': pages, 'sha256': digest(path.read_bytes()),
            'scope': 'same pinned PDFium 96dpi RGBA raster, diagnostic-cloned-dom, not application PDF consumer'}


def first_difference(left, right, path='$'):
    if type(left) is not type(right):
        return {'path': path, 'electron': left, 'wkwebview': right}
    if isinstance(left, dict):
        if left.keys() != right.keys():
            return {'path': path, 'electronKeys': sorted(left), 'wkwebviewKeys': sorted(right)}
        for key in left:
            found = first_difference(left[key], right[key], f'{path}.{key}')
            if found is not None:
                return found
    elif isinstance(left, list):
        if len(left) != len(right):
            return {'path': path, 'electronLength': len(left), 'wkwebviewLength': len(right)}
        for index, (a, b) in enumerate(zip(left, right)):
            found = first_difference(a, b, f'{path}[{index}]')
            if found is not None:
                return found
    elif left != right:
        return {'path': path, 'electron': left, 'wkwebview': right}
    return None


def comparison_facts(result):
    native = result['native']
    outcome = native['outcome']
    facts = {'input': outcome['input'], 'layout': outcome['layout'],
             'editedLayout': outcome['editedLayout'], 'cases': outcome['cases']}
    return facts


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--electron', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != 'darwin':
        raise RuntimeError('This diagnostic requires a fresh hosted macOS runner')
    if args.output.exists():
        raise FileExistsError('Output must be a new directory; no reuse or deletion')
    args.output.mkdir(parents=True)
    site = args.output / 'site'
    site.mkdir()
    shutil.copytree(args.repo / 'apps/web/dist/docs', site / 'docs')
    shutil.copytree(args.repo / 'packages/ui/template-fonts', site / 'template-fonts')
    shutil.copytree(args.probe / 'frontend/probe', site / 'probe')
    fixture = site / 'probe/fixture.docx'
    with (args.output / 'fixture-generator.log').open('wb') as log:
        subprocess.run([sys.executable, str(args.probe / 'frontend/make_fixture.py'), str(fixture)],
                       check=True, stdout=log, stderr=subprocess.STDOUT)
    inputs = []
    for path in sorted(site.rglob('*')):
        if path.is_file():
            body = path.read_bytes()
            inputs.append({'path': path.relative_to(site).as_posix(), 'bytes': len(body), 'sha256': digest(body)})
    write_json(args.output / 'built-inputs.json', inputs)
    server = ThreadingHTTPServer(('127.0.0.1', 0), partial(SimpleHTTPRequestHandler, directory=str(site)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    results = {}
    errors = []
    try:
        base = f'http://127.0.0.1:{server.server_port}/probe/index.html?engine='
        commands = {
            'electron': [str(args.electron.resolve()), str(args.probe / 'native/electron.cjs'), base + 'electron'],
            'wkwebview': [sys.executable, str(args.probe / 'native/wkwebview.py'), base + 'wkwebview'],
        }
        for name, command in commands.items():
            output = args.output / name
            result = run_driver(command + [str(output)], output)
            results[name] = result
            try:
                if result['returncode'] != 0 or result['native']['status'] != 'complete':
                    raise ValueError('native diagnostic did not complete')
                native = result['native']
                if native['outcome']['status'] != 'complete':
                    raise ValueError('renderer diagnostic did not complete')
                if native['screenshot']['status'] != 'complete' or native['pdf']['status'] != 'complete':
                    raise ValueError('actual screenshot or PDF did not complete')
                png = (output / 'screenshot.png').read_bytes()
                if png[:8] != b'\x89PNG\r\n\x1a\n':
                    raise ValueError('native screenshot is not PNG')
                result['savedVerification'] = verify_saved(result, output, fixture)
                result['pdfInspection'] = inspect_pdf(output / 'document.pdf')
            except Exception as error:
                errors.append({'driver': name, 'error': str(error)})
            write_json(output / 'observed-result.json', result)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    differences = {}
    if not errors:
        a, b = results['electron'], results['wkwebview']
        differences['renderer'] = first_difference(comparison_facts(a), comparison_facts(b))
        raster_facts = lambda result: [{key: page[key] for key in ('pageSize', 'pixelSize', 'pixelSha256')}
                                      for page in result['pdfInspection']['pageFacts']]
        differences['pdf'] = first_difference(raster_facts(a), raster_facts(b))
    state = 'BLOCKED' if errors else ('DIFFERENT' if any(differences.values()) else 'MATCH')
    report = {'state': state, 'errors': errors, 'firstDifferences': differences,
              'builtInputManifestSha256': digest((args.output / 'built-inputs.json').read_bytes()),
              'fixtureSha256': digest(fixture.read_bytes()),
              'productGreen': False, 'candidateBuilt': False, 'nativeAuthorityVerified': False,
              'limits': 'One synthetic document/current renderer per engine. No performance claim, native filesystem save proof, actual app PDF consumer or release acceptance. Screenshots retain native DPR; no normalized pixel parity claim.'}
    write_json(args.output / 'comparison.json', report)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if state == 'MATCH' else 1


if __name__ == '__main__':
    raise SystemExit(main())
