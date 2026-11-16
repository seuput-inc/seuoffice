using System.Text.Json;
using Microsoft.Web.WebView2.Core;
using Microsoft.Web.WebView2.WinForms;

internal static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        if (args.Length != 1) throw new ArgumentException("Supply a fresh output directory.");
        var output = Path.GetFullPath(args[0]);
        if (Directory.Exists(output)) throw new IOException("Output directory already exists.");
        Directory.CreateDirectory(output);
        ApplicationConfiguration.Initialize();
        Application.Run(new Probe(output));
    }
}

internal sealed class Probe : Form
{
    private readonly string output;
    private readonly List<string> messages = new();
    private readonly WebView2 view = new() { Dock = DockStyle.Fill };
    private int sequence;
    private bool browserExited;

    internal Probe(string output)
    {
        this.output = output;
        Controls.Add(view);
        ShowInTaskbar = false;
        Width = 400;
        Height = 300;
        Shown += async (_, _) => await Observe();
    }

    private void Event(string kind, object? details = null)
    {
        if (sequence >= 500) return;
        File.AppendAllText(Path.Combine(output, "events.jsonl"),
            JsonSerializer.Serialize(new { seq = ++sequence, utc = DateTimeOffset.UtcNow, kind, details }) + "\n");
    }

    private async Task WaitFor(string message, bool prefix = false)
    {
        for (var i = 0; i < 50; i++)
        {
            if (messages.Any(value => prefix ? value.StartsWith(message, StringComparison.Ordinal) : value == message)) return;
            await Task.Delay(100);
        }
        Event("timeout", new { message, prefix });
        throw new TimeoutException("Synthetic fixture message missing: " + message);
    }

    private void Message(string callback, object? sender, CoreWebView2WebMessageReceivedEventArgs args, CoreWebView2Frame? frame = null)
    {
        string body;
        try { body = args.TryGetWebMessageAsString(); }
        catch (Exception ex) { Event("message_decode_error", new { callback, error = ex.ToString() }); return; }
        messages.Add(body);
        Event("web_message", new {
            callback,
            nativeSource = args.Source,
            nativeSenderMatchesView = ReferenceEquals(sender, view.CoreWebView2),
            nativeSenderMatchesFrame = frame != null && ReferenceEquals(sender, frame),
            nativeFrameId = frame?.FrameId,
            mainFrame = callback == "CoreWebView2.WebMessageReceived" ? true : (bool?)false,
            body
        });
        if (frame != null && body == "frame:request")
        {
            try
            {
                frame.PostWebMessageAsJson("\"frame-current-response\"");
                Event("frame_current_post_returned", new { nativeFrameId = frame.FrameId });
            }
            catch (Exception ex) { Event("frame_current_post_error", new { error = ex.ToString() }); }
        }
    }

    private async Task Observe()
    {
        var status = "incomplete";
        string? error = null;
        try
        {
            Event("start", new { sdk = "1.0.2903.40", output, processId = Environment.ProcessId });
            var env = await CoreWebView2Environment.CreateAsync(null, Path.Combine(output, "user-data"));
            Event("runtime", new { version = env.BrowserVersionString });
            env.BrowserProcessExited += (_, e) => {
                browserExited = true;
                Event("browser_process_exited", new { kind = e.BrowserProcessExitKind.ToString(), pid = e.BrowserProcessId });
            };
            await view.EnsureCoreWebView2Async(env);
            var core = view.CoreWebView2;
            Event("native_view", new { browserProcessId = core.BrowserProcessId });
            core.ProcessFailed += (_, e) => Event("process_failed", new { kind = e.ProcessFailedKind.ToString() });
            core.NavigationStarting += (_, e) => Event("navigation_starting", new { e.Uri, e.NavigationId, e.IsUserInitiated, e.IsRedirected });
            core.NavigationCompleted += (_, e) => Event("navigation_completed", new { e.NavigationId, e.IsSuccess, error = e.WebErrorStatus.ToString() });
            core.SourceChanged += (_, e) => Event("source_changed", new { core.Source, e.IsNewDocument });
            core.WebMessageReceived += (sender, e) => Message("CoreWebView2.WebMessageReceived", sender, e);
            core.FrameCreated += (_, e) => {
                var frame = e.Frame;
                var frameId = frame.FrameId;
                Event("frame_created", new { nativeFrameId = frameId, nativeName = frame.Name });
                frame.Destroyed += (sender, _) => Event("frame_destroyed", new { nativeFrameId = frameId, nativeSenderMatchesFrame = ReferenceEquals(sender, frame) });
                frame.WebMessageReceived += (sender, args) => Message("CoreWebView2Frame.WebMessageReceived", sender, args, frame);
            };

            core.NavigateToString("""
                <!doctype html><meta charset="utf-8"><title>synthetic A</title><body>
                <script>
                window.originalSentinel = 'A-window';
                chrome.webview.addEventListener('message', e => chrome.webview.postMessage('A:received:' + e.data));
                chrome.webview.postMessage('A:request');
                const f = document.createElement('iframe');
                f.srcdoc = `<script>chrome.webview.addEventListener('message', e => chrome.webview.postMessage('frame:received:' + e.data)); chrome.webview.postMessage('frame:request');<\/script>`;
                document.body.appendChild(f);
                </script>
                """);
            await WaitFor("A:request");
            await WaitFor("frame:request");
            await WaitFor("frame:received:frame-current-response");
            Event("held_A_request", new { replyHandle = "unavailable", note = "No request-bound reply method is exposed by WebMessageReceivedEventArgs." });
            await core.ExecuteScriptAsync("location.hash = 'fragment'; chrome.webview.postMessage('A:fragment:' + location.hash)");
            await WaitFor("A:fragment:#fragment");
            var rewritten = "<!doctype html><title>rewritten A</title><script>chrome.webview.addEventListener('message',e=>chrome.webview.postMessage('rewrite:received:'+e.data));chrome.webview.postMessage('rewrite:sentinel:'+window.originalSentinel);</script>";
            await core.ExecuteScriptAsync("document.open();document.write(" + JsonSerializer.Serialize(rewritten) + ");document.close();");
            await WaitFor("rewrite:sentinel:", prefix: true);
            core.PostWebMessageAsJson("\"after-document-open\"");
            Event("current_top_level_post_returned", new { phase = "after_document_open", originalReply = false });
            await Task.Delay(500);
            core.NavigateToString("""
                <!doctype html><meta charset="utf-8"><title>synthetic B</title><script>
                chrome.webview.addEventListener('message', e => chrome.webview.postMessage('B:received:' + e.data));
                chrome.webview.postMessage('B:ready');
                </script>
                """);
            await WaitFor("B:ready");
            core.PostWebMessageAsJson("\"delayed-for-A-current-view-only\"");
            Event("current_top_level_post_returned", new { phase = "B_after_held_A", originalReply = false });
            await Task.Delay(1000);
            Event("delivery_window_ended", new { milliseconds = 1000 });
            Controls.Remove(view);
            Event("view_detached", new { browserExited });
            view.Dispose();
            Event("view_disposed", new { browserExited });
            try
            {
                core.PostWebMessageAsJson("\"after-close-current-view-only\"");
                Event("post_after_dispose_returned", new { originalReply = false });
            }
            catch (Exception ex) { Event("post_after_dispose_error", new { error = ex.ToString() }); }
            for (var i = 0; i < 50 && !browserExited; i++) await Task.Delay(100);
            Event("process_exit_observation_end", new { browserExited, milliseconds = 5000 });
            Event("finished", new { note = "Observation only. No original-document reply API was exercised." });
            status = "observed";
        }
        catch (Exception ex)
        {
            error = ex.ToString();
            Event("observer_error", new { error });
            Environment.ExitCode = 1;
        }
        finally
        {
            File.WriteAllText(Path.Combine(output, "result.json"), JsonSerializer.Serialize(new {
                probe = "webview2-original-reply",
                status,
                error,
                eventCount = sequence,
                originalReplyApiExercised = false,
                productGreen = false
            }));
            Close();
        }
    }
}
