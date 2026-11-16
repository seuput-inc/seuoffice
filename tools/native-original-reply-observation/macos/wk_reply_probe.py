#!/usr/bin/env python3
"""Throwaway WKWebView reply observation. Run only on an isolated hosted macOS runner."""
import json
import platform
import sys
import time
from pathlib import Path

if len(sys.argv) != 2:
    raise SystemExit('usage: python3 wk_reply_probe.py OUTPUT_DIR')

output = Path(sys.argv[1])
output.mkdir(parents=True, exist_ok=False)
events_path = output / 'events.jsonl'
result_path = output / 'result.json'
started = time.monotonic()
result = {
    'probe': 'wkwebview-original-reply',
    'status': 'incomplete',
    'os': platform.mac_ver()[0],
    'events': 'events.jsonl',
    'identity_note': 'WKFrameInfo is transient, not a persistent frame or document identity. JS labels are claims, not native identity.',
    'termination_note': 'Window close and view detach are not web content process termination. Termination is only recorded if WebKit reports it.',
    'observation_window_seconds': 2,
}
count = 0
received = set()
held = {}
window = None
webview = None
navigation_delegate = None

with events_path.open('w', encoding='utf-8') as events:
    def emit(event, **fields):
        global count
        if count >= 256:
            result['events_truncated'] = True
            return
        count += 1
        record = {'seq': count, 'elapsed_ms': round((time.monotonic() - started) * 1000), 'event': event, **fields}
        events.write(json.dumps(record, ensure_ascii=True, default=str) + '\n')
        events.flush()

    def persist():
        result['event_count'] = count
        result_path.write_text(json.dumps(result, indent=2, ensure_ascii=True) + '\n', encoding='utf-8')

    try:
        try:
            import objc
            from AppKit import (NSApplication, NSBackingStoreBuffered, NSMakeRect,
                                NSView, NSWindow, NSWindowStyleMaskTitled)
            from Foundation import NSDate, NSObject, NSRunLoop, NSURL
            from WebKit import WKContentWorld, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore
            reply_protocol = objc.protocolNamed('WKScriptMessageHandlerWithReply')
        except (ImportError, AttributeError, objc.error if 'objc' in globals() else ImportError) as error:
            result.update(status='unavailable', error=f'{type(error).__name__}: {error}')
            raise SystemExit(2)

        class Handler(NSObject, protocols=[reply_protocol]):
            def userContentController_didReceiveScriptMessage_replyHandler_(self, controller, message, reply):
                body = message.body()
                kind = str(body.get('kind', 'invalid'))[:80] if hasattr(body, 'get') else 'invalid'
                label = str(body.get('label', ''))[:160] if hasattr(body, 'get') else ''
                detail = str(body.get('detail', ''))[:300] if hasattr(body, 'get') else ''
                frame = message.frameInfo()
                sender_view = message.webView()
                frame_view = frame.webView()
                request_url = frame.request().URL()
                emit('script_message', kind=kind, js_claim_label=label, js_claim_detail=detail,
                     native_sender_webview=(hex(objc.pyobjc_id(sender_view)) if sender_view else None),
                     native_frame_info_object=hex(objc.pyobjc_id(frame)),
                     native_frame_webview=(hex(objc.pyobjc_id(frame_view)) if frame_view else None),
                     native_sender_is_probe_webview=(sender_view is not None and objc.pyobjc_id(sender_view) == objc.pyobjc_id(webview)),
                     native_is_main_frame=bool(frame.isMainFrame()),
                     native_frame_request_url=(str(request_url.absoluteString())[:300] if request_url else None))
                received.add(kind)
                if kind in ('hold-fragment', 'hold-open', 'hold-cross', 'hold-detach', 'hold-close'):
                    if kind in held:
                        emit('duplicate_hold', kind=kind)
                        reply(None, 'duplicate probe hold')
                    else:
                        held[kind] = reply
                        emit('reply_held', kind=kind)
                elif kind == 'open-barrier' and 'hold-open' not in held:
                    emit('barrier_failed', kind=kind)
                    reply(None, 'hold-open not yet received')
                else:
                    reply('ack:' + kind, None)

        class NavigationDelegate(NSObject):
            def webView_didStartProvisionalNavigation_(self, view, navigation):
                emit('navigation_started', native_webview=hex(objc.pyobjc_id(view)))

            def webView_didCommitNavigation_(self, view, navigation):
                emit('navigation_committed', native_webview=hex(objc.pyobjc_id(view)))

            def webView_didFinishNavigation_(self, view, navigation):
                emit('navigation_finished', native_webview=hex(objc.pyobjc_id(view)))

            def webView_didFailNavigation_withError_(self, view, navigation, error):
                emit('navigation_failed', error=str(error)[:300])

            def webView_didFailProvisionalNavigation_withError_(self, view, navigation, error):
                emit('provisional_navigation_failed', error=str(error)[:300])

            def webViewWebContentProcessDidTerminate_(self, view):
                emit('web_content_process_terminated', native_webview=hex(objc.pyobjc_id(view)))

        application = NSApplication.sharedApplication()
        config = WKWebViewConfiguration.alloc().init()
        config.setWebsiteDataStore_(WKWebsiteDataStore.nonPersistentDataStore())
        controller = config.userContentController()
        selector = b'addScriptMessageHandlerWithReply:contentWorld:name:'
        if not controller.respondsToSelector_(selector):
            result.update(status='unavailable', error='WKScriptMessageHandlerWithReply registration unavailable')
            raise SystemExit(2)
        handler = Handler.alloc().init()
        controller.addScriptMessageHandlerWithReply_contentWorld_name_(handler, WKContentWorld.pageWorld(), 'probe')
        webview = WKWebView.alloc().initWithFrame_configuration_(NSMakeRect(0, 0, 640, 480), config)
        window = NSWindow.alloc().initWithContentRect_styleMask_backing_defer_(
            NSMakeRect(0, 0, 640, 480), NSWindowStyleMaskTitled, NSBackingStoreBuffered, False)
        window.setReleasedWhenClosed_(False)
        container = NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 640, 480))
        window.setContentView_(container)
        container.addSubview_(webview)
        window.makeKeyAndOrderFront_(None)
        navigation_delegate = NavigationDelegate.alloc().init()
        webview.setNavigationDelegate_(navigation_delegate)
        emit('initialized', native_probe_webview=hex(objc.pyobjc_id(webview)), data_store='nonpersistent')

        def pump_until(predicate, seconds, description):
            deadline = time.monotonic() + seconds
            while not predicate():
                if time.monotonic() >= deadline:
                    raise TimeoutError(description)
                NSRunLoop.currentRunLoop().runUntilDate_(NSDate.dateWithTimeIntervalSinceNow_(0.05))

        def observe(seconds, label):
            emit('observation_window_start', label=label, seconds=seconds)
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                NSRunLoop.currentRunLoop().runUntilDate_(NSDate.dateWithTimeIntervalSinceNow_(0.05))
            emit('observation_window_end', label=label)

        def release(kind):
            callback = held.pop(kind)
            emit('reply_call_start', kind=kind)
            callback('reply:' + kind, None)
            emit('reply_call_returned', kind=kind)

        replacement = r'''<!doctype html><meta charset="utf-8"><script>(() => {
const post = (kind, label, detail) => window.webkit.messageHandlers.probe.postMessage({kind, label, detail: String(detail || '')});
const report = (kind, label, detail) => { post(kind, label, detail).catch(() => {}); };
report('open-ready', 'A-after-document-open', 'new markup executing');
post('hold-cross', 'A-after-document-open', '').then(
  value => report('cross-settled', 'A-after-document-open', value),
  error => report('cross-rejected', 'A-after-document-open', error));
})();</script>'''
        replacement_literal = json.dumps(replacement).replace('</', '<\\/')
        frame = r'''<!doctype html><script>
window.webkit.messageHandlers.probe.postMessage({kind:'basic-frame',label:'A-iframe'}).then(
 value => window.webkit.messageHandlers.probe.postMessage({kind:'basic-frame-settled',label:'A-iframe',detail:String(value)}));
</script>'''
        frame_literal = json.dumps(frame).replace('</', '<\\/')
        a = r'''<!doctype html><meta charset="utf-8"><script>(() => {
const post = (kind, label, detail) => window.webkit.messageHandlers.probe.postMessage({kind, label, detail: String(detail || '')});
const report = (kind, label, detail) => { post(kind, label, detail).catch(() => {}); };
post('basic-main', 'A-main', '').then(value => report('basic-main-settled', 'A-main', value));
const f = document.createElement('iframe'); f.srcdoc = FRAME_LITERAL; document.documentElement.appendChild(f);
post('hold-fragment', 'A-before-fragment', '').then(value => {
  report('fragment-settled', 'A-after-fragment', value);
  post('hold-open', 'A-before-document-open', '').then(
    value => report('open-settled', 'A-old-closure', value),
    error => report('open-rejected', 'A-old-closure', error));
  post('open-barrier', 'A-before-document-open', '').then(() => {
    document.open(); document.write(REPLACEMENT_LITERAL); document.close();
  }, error => report('open-barrier-rejected', 'A', error));
}, error => report('fragment-rejected', 'A', error));
location.hash = 'fragment';
report('fragment-ready', 'A-after-fragment', location.hash);
})();</script>'''.replace('FRAME_LITERAL', frame_literal).replace('REPLACEMENT_LITERAL', replacement_literal)
        b = r'''<!doctype html><meta charset="utf-8"><script>(() => {
const post = (kind, label, detail) => window.webkit.messageHandlers.probe.postMessage({kind, label, detail: String(detail || '')});
const report = (kind, label, detail) => { post(kind, label, detail).catch(() => {}); };
window.addEventListener('message', e => report('B-message-event', 'B', e.data));
window.addEventListener('unhandledrejection', e => report('B-unhandled-rejection', 'B', e.reason));
report('B-ready', 'B', 'new document executing');
for (const kind of ['hold-detach', 'hold-close']) {
  post(kind, 'B-before-' + kind, '').then(
    value => report(kind + '-settled', 'B', value),
    error => report(kind + '-rejected', 'B', error));
}
})();</script>'''
        emit('load_A')
        webview.loadHTMLString_baseURL_(a, NSURL.URLWithString_('https://fixture.invalid/a'))
        pump_until(lambda: {'basic-main', 'basic-frame', 'basic-main-settled', 'basic-frame-settled',
                            'hold-fragment', 'fragment-ready'} <= received,
                   15, 'A main, iframe, or fragment messages missing')
        release('hold-fragment')
        pump_until(lambda: {'hold-open', 'open-ready', 'hold-cross'} <= received,
                   15, 'document.open or cross-navigation hold missing')
        release('hold-open')
        observe(2, 'after-document-open-reply')
        emit('load_B_cross_document')
        webview.loadHTMLString_baseURL_(b, NSURL.URLWithString_('https://fixture.invalid/b'))
        pump_until(lambda: {'B-ready', 'hold-detach', 'hold-close'} <= received,
                   15, 'B or lifecycle holds missing')
        release('hold-cross')
        observe(2, 'after-cross-document-reply')
        emit('detach_view_start')
        webview.removeFromSuperview()
        emit('detach_view_returned', native_superview_present=bool(webview.superview()))
        release('hold-detach')
        observe(2, 'after-detach-reply')
        emit('close_window_start')
        window.close()
        emit('close_window_returned', native_webview_retained=True)
        release('hold-close')
        observe(2, 'after-close-reply')
        result['status'] = 'observed'
        result['received_message_kinds'] = sorted(received)
        result['interpretation'] = 'Observed means the bounded sequence ran, not that any delivery or security property passed.'
    except SystemExit:
        pass
    except Exception as error:
        result.update(status='incomplete', error=f'{type(error).__name__}: {error}')
        emit('probe_error', error=result['error'])
    finally:
        if webview is not None:
            webview.stopLoading()
            webview.setNavigationDelegate_(None)
        if window is not None:
            window.close()
        persist()

sys.exit(0 if result['status'] == 'observed' else (2 if result['status'] == 'unavailable' else 1))
