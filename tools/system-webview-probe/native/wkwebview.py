#!/usr/bin/env python3
import json
import platform
import sys
import time
from pathlib import Path
from urllib.parse import urlparse


output = Path(sys.argv[2]).resolve() if len(sys.argv) >= 3 else None
outcome = None
result = {'driver': 'wkwebview', 'status': 'failed', 'outcome': None,
          'screenshot': {'status': 'failed', 'path': 'screenshot.png'},
          'pdf': {'status': 'failed', 'path': 'document.pdf'}}
window = None
webview = None

try:
    if len(sys.argv) != 3:
        raise ValueError('Usage: python3 wkwebview.py URL OUTPUT_DIR')
    output.mkdir(parents=True, exist_ok=True)
    parsed = urlparse(sys.argv[1])
    if parsed.scheme != 'http' or parsed.hostname not in ('localhost', '127.0.0.1', '::1'):
        raise ValueError('URL must use loopback HTTP')

    import objc
    from AppKit import (NSApplication, NSBackingStoreBuffered, NSBitmapImageRep,
                        NSMakeRect, NSPNGFileType, NSPrintInfo, NSPrintJobSavingURL,
                        NSPrintSaveJob, NSWindow, NSWindowStyleMaskTitled)
    from Foundation import NSDate, NSObject, NSRunLoop, NSURL, NSURLRequest
    from WebKit import WKWebView, WKWebViewConfiguration, WKWebsiteDataStore

    deadline = time.monotonic() + 180
    callbacks = []
    navigation = {'error': None}

    class NavigationDelegate(NSObject):
        def webView_didFailNavigation_withError_(self, view, nav, error):
            navigation['error'] = str(error)

        def webView_didFailProvisionalNavigation_withError_(self, view, nav, error):
            navigation['error'] = str(error)

        def webViewWebContentProcessDidTerminate_(self, view):
            navigation['error'] = 'WKWebView content process terminated'

    class PrintDelegate(NSObject):
        @objc.signature(b'v@:@B^v')
        def printOperationDidRun_success_contextInfo_(self, operation, success, context):
            self.result = bool(success)

    def spin_until(predicate):
        while not predicate():
            if navigation['error']:
                raise RuntimeError(navigation['error'])
            if time.monotonic() >= deadline:
                raise TimeoutError('Diagnostic deadline exceeded (180 seconds)')
            NSRunLoop.currentRunLoop().runUntilDate_(NSDate.dateWithTimeIntervalSinceNow_(0.05))

    def evaluate(script):
        state = {'done': False}

        def completed(value, error):
            state.update(done=True, value=value, error=error)

        callbacks.append(completed)
        webview.evaluateJavaScript_completionHandler_(script, completed)
        spin_until(lambda: state['done'])
        if state['error'] is not None:
            raise RuntimeError(f'JavaScript evaluation failed: {state["error"]}')
        return state['value']

    application = NSApplication.sharedApplication()
    config = WKWebViewConfiguration.alloc().init()
    config.setWebsiteDataStore_(WKWebsiteDataStore.nonPersistentDataStore())
    webview = WKWebView.alloc().initWithFrame_configuration_(NSMakeRect(0, 0, 1280, 900), config)
    window = NSWindow.alloc().initWithContentRect_styleMask_backing_defer_(
        NSMakeRect(0, 0, 1280, 900), NSWindowStyleMaskTitled, NSBackingStoreBuffered, False)
    window.setReleasedWhenClosed_(False)
    window.setContentView_(webview)
    window.makeKeyAndOrderFront_(None)
    delegate = NavigationDelegate.alloc().init()
    webview.setNavigationDelegate_(delegate)
    webview.loadRequest_(NSURLRequest.requestWithURL_(NSURL.URLWithString_(sys.argv[1])))
    spin_until(lambda: not webview.isLoading() and webview.URL() is not None)

    while True:
        if time.monotonic() >= deadline:
            raise TimeoutError('Diagnostic deadline exceeded (180 seconds)')
        raw = evaluate('JSON.stringify(window.__probeOutcome)')
        if raw is not None:
            outcome = json.loads(str(raw))
            result['outcome'] = outcome
            status = outcome.get('status')
            if status == 'failed':
                raise RuntimeError(f'Probe failed: {outcome.get("error")}')
            if status == 'complete':
                break
            if status != 'running':
                raise RuntimeError(f'Unexpected probe status: {status}')
        spin_until_time = min(deadline, time.monotonic() + 0.25)
        spin_until(lambda: time.monotonic() >= spin_until_time)

    viewport = json.loads(str(evaluate('JSON.stringify({width: innerWidth, height: innerHeight, devicePixelRatio, userAgent: navigator.userAgent})')))
    if viewport['width'] != 1280 or viewport['height'] != 900:
        raise RuntimeError(f'Unexpected viewport: {viewport}')
    snapshot = {'done': False}

    def snapshot_done(image, error):
        snapshot.update(done=True, image=image, error=error)

    callbacks.append(snapshot_done)
    webview.takeSnapshotWithConfiguration_completionHandler_(None, snapshot_done)
    spin_until(lambda: snapshot['done'])
    if snapshot['error'] is not None or snapshot['image'] is None:
        raise RuntimeError(f'WKWebView snapshot failed: {snapshot["error"]}')
    bitmap = NSBitmapImageRep.imageRepWithData_(snapshot['image'].TIFFRepresentation())
    if bitmap is None or bitmap.pixelsWide() <= 0 or bitmap.pixelsHigh() <= 0:
        raise RuntimeError('WKWebView snapshot has no pixels')
    png = bitmap.representationUsingType_properties_(NSPNGFileType, {})
    if png is None or not png.writeToFile_atomically_(str(output / 'screenshot.png'), True):
        raise RuntimeError('Could not write WKWebView snapshot')
    result['screenshot'] = {
        'status': 'complete', 'path': 'screenshot.png', 'os': platform.mac_ver()[0],
        'osRelease': platform.release(), 'viewport': viewport,
        'imagePixels': {'width': bitmap.pixelsWide(), 'height': bitmap.pixelsHigh()},
    }

    evaluate("window.__probePrintResult = null; Promise.resolve().then(() => window.__prepareProbePrint()).then(value => { window.__probePrintResult = JSON.stringify({value}); }, error => { window.__probePrintResult = JSON.stringify({error: String(error && error.stack || error)}); }); true")
    prepared = None
    while prepared is None:
        if time.monotonic() >= deadline:
            raise TimeoutError('Diagnostic deadline exceeded (180 seconds)')
        raw = evaluate('window.__probePrintResult')
        if raw is not None:
            prepared = json.loads(str(raw))
            break
        spin_until_time = min(deadline, time.monotonic() + 0.05)
        spin_until(lambda: time.monotonic() >= spin_until_time)
    if 'error' in prepared:
        raise RuntimeError(f'Print preparation failed: {prepared["error"]}')
    print_preparation = json.loads(prepared['value'])

    info = NSPrintInfo.alloc().init()
    info.setPaperSize_((210 * 72 / 25.4, 297 * 72 / 25.4))
    info.setTopMargin_(0)
    info.setBottomMargin_(0)
    info.setLeftMargin_(0)
    info.setRightMargin_(0)
    info.setJobDisposition_(NSPrintSaveJob)
    info.dictionary()[NSPrintJobSavingURL] = NSURL.fileURLWithPath_(str(output / 'document.pdf'))
    operation = webview.printOperationWithPrintInfo_(info)
    if operation is None:
        raise RuntimeError('WKWebView did not provide an NSPrintOperation')
    print_view = operation.view()
    if print_view is None:
        raise RuntimeError('NSPrintOperation has no print view')
    print_view.setFrame_(NSMakeRect(0, 0, 210 * 72 / 25.4, 297 * 72 / 25.4))
    operation.setShowsPrintPanel_(False)
    operation.setShowsProgressPanel_(False)
    print_delegate = PrintDelegate.alloc().init()
    print_delegate.result = None
    operation.runOperationModalForWindow_delegate_didRunSelector_contextInfo_(
        window, print_delegate, b'printOperationDidRun:success:contextInfo:', None)
    spin_until(lambda: print_delegate.result is not None)
    if not print_delegate.result:
        raise RuntimeError('NSPrintOperation reported failure')
    pdf_path = output / 'document.pdf'
    with pdf_path.open('rb') as pdf:
        if pdf.read(5) != b'%PDF-':
            raise RuntimeError('NSPrintOperation did not produce a PDF')
    result['pdf'] = {'status': 'complete', 'path': 'document.pdf',
                     'printMode': 'diagnostic-cloned-dom', 'printPreparation': print_preparation}
    result['status'] = 'complete'
    exit_code = 0
except Exception as error:
    exit_code = 1
    result['error'] = f'{type(error).__name__}: {error}'
    print(result['error'], file=sys.stderr)
finally:
    if output is not None:
        try:
            output.mkdir(parents=True, exist_ok=True)
            (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
        except Exception as error:
            exit_code = 1
            print(f'Cannot write result.json: {error}', file=sys.stderr)
    if webview is not None:
        webview.stopLoading()
        webview.setNavigationDelegate_(None)
    if window is not None:
        window.close()

sys.exit(exit_code)
