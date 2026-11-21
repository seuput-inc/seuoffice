import AppKit
import ApplicationServices

// This helper only drives the installed QA product. It does not request TCC access.
func stop(_ reason: String, _ code: Int32 = 1) -> Never {
    if code == 3 { FileHandle.standardOutput.write(Data("G6_BLOCKED \(reason)\n".utf8)) }
    FileHandle.standardError.write(Data("G6 macOS UI: \(reason)\n".utf8))
    exit(code)
}
let env = ProcessInfo.processInfo.environment
let args = CommandLine.arguments
guard env["GITHUB_ACTIONS"] == "true", env["RUNNER_ENVIRONMENT"] == "github-hosted",
      env["RUNNER_OS"] == "macOS", env["GITHUB_REPOSITORY"] == "seuput-inc/seuoffice",
      env["GITHUB_EVENT_NAME"] == "workflow_dispatch", env["GITHUB_REF"] == "refs/heads/integration/tauri-first-candidate",
      env["G6_PLAN"] == "/Users/runner/work/_temp/g6-qa/plan.json", NSUserName() == "runner",
      args.count == 5, ["seed", "arrival", "update"].contains(args[1]), let pid = Int32(args[2]),
      let app = NSRunningApplication(processIdentifier: pid), app.bundleIdentifier == "com.seuput.seuoffice",
      app.executableURL?.path == "/Applications/SeuOffice.app/Contents/MacOS/SeuOffice" else {
    stop("expected the hosted runner and verified installed application", 3)
}
guard AXIsProcessTrusted() else { stop("Accessibility is not granted to the runner", 3) }
let target = AXUIElementCreateApplication(pid)
AXUIElementSetMessagingTimeout(target, 3)
// Chromium uses this application-local AX attribute to expose its web content
// to an already authorized accessibility client. It grants no system permission.
_ = AXUIElementSetAttributeValue(target, "AXManualAccessibility" as CFString, kCFBooleanTrue)
func attribute(_ element: AXUIElement, _ key: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, key as CFString, &value) == .success ? value : nil
}
func windows() -> [AXUIElement] {
    (attribute(target, "AXWindows") as? [AXUIElement] ?? []).filter { attribute($0, "AXMinimized") as? Bool != true }
}
func find(_ roots: [AXUIElement], _ roles: Set<String>, _ matches: (AXUIElement, String) -> Bool) -> AXUIElement? {
    var nodes = roots
    var index = 0
    while index < nodes.count && index < 6000 {
        let node = nodes[index]; index += 1
        guard let role = attribute(node, "AXRole") as? String, attribute(node, "AXHidden") as? Bool != true else { continue }
        if roles.contains(role), matches(node, role) { return node }
        // Never inspect entry fields, including AI credentials in Settings.
        if ["AXTextField", "AXTextArea", "AXSecureTextField"].contains(role) { continue }
        if let children = attribute(node, "AXChildren") as? [AXUIElement] { nodes.append(contentsOf: children) }
    }
    return nil
}
func named(_ roots: [AXUIElement], _ roles: Set<String>, _ names: [String]) -> AXUIElement? {
    find(roots, roles) { node, role in
        let keys = role == "AXStaticText" ? ["AXValue", "AXTitle", "AXDescription"] : ["AXTitle", "AXDescription"]
        return keys.contains { key in
            guard let value = attribute(node, key) as? String else { return false }
            return names.contains(value)
        }
    }
}
func press(_ node: AXUIElement, _ label: String) {
    guard attribute(node, "AXEnabled") as? Bool != false,
          AXUIElementPerformAction(node, kAXPressAction as CFString) == .success else {
        stop("exposed control could not be pressed: \(label)", 3)
    }
}
func until(_ seconds: Double, _ reason: String, code: Int32 = 1, _ body: () -> Bool) {
    let end = ProcessInfo.processInfo.systemUptime + seconds
    repeat {
        if let front = NSWorkspace.shared.frontmostApplication?.bundleIdentifier,
           ["com.apple.SecurityAgent", "com.apple.LocalAuthentication.UIAgent"].contains(front) {
            stop("a system authorization prompt requires user intervention", 3)
        }
        guard !app.isTerminated else { stop("installed application exited while waiting for \(reason)") }
        if body() { return }
        Thread.sleep(forTimeInterval: 0.4)
    } while ProcessInfo.processInfo.systemUptime < end
    stop("timed out waiting for \(reason)", code)
}
let button: Set<String> = ["AXButton"]
let text: Set<String> = ["AXStaticText", "AXHeading"]
func click(_ labels: [String]) {
    until(30, labels[0]) {
        guard let node = named(windows(), button, labels) else { return false }
        press(node, labels[0]); return true
    }
}
func checkMenu() {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(target, kAXMenuBarAttribute as CFString, &value) == .success,
          let value, CFGetTypeID(value) == AXUIElementGetTypeID() else { stop("no native menu bar", 3) }
    let bar = value as! AXUIElement
    guard let help = named([bar], ["AXMenuBarItem"], ["Help", "도움말"]) else { stop("Help menu unavailable", 3) }
    press(help, "Help")
    until(10, "Help > Check for Updates", code: 3) {
        guard let action = named([help], ["AXMenuItem"], ["Check for Updates…", "업데이트 확인…"]) else { return false }
        press(action, "Check for Updates"); return true
    }
}
func updateCard() -> AXUIElement? {
    windows().first { window in
        let titles = ["Software Update", "소프트웨어 업데이트"]
        return titles.contains(attribute(window, "AXTitle") as? String ?? "") ||
            named([window], ["AXWebArea"], titles) != nil
    }
}

until(60, "the installed product's accessible renderer", code: 3) {
    find(windows(), ["AXWebArea"]) { _, _ in true } != nil
}
if args[1] == "update" {
    let current = "v" + args[3], offered = "v" + args[4]
    if updateCard() == nil { checkMenu() }
    var downloadPressed = false
    until(300, "the exact offered version and Restart & Install") {
        guard let card = updateCard(), named([card], text, [current]) != nil,
              named([card], text, [offered]) != nil else { return false }
        if let install = named([card], button, ["Restart & Install", "다시 시작 및 설치"]) {
            // The app can quit before answering AXPress. Never send it twice;
            // the caller must still see a new process running the exact code.
            let result = AXUIElementPerformAction(install, kAXPressAction as CFString)
            guard [.success, .cannotComplete, .invalidUIElement].contains(result) else {
                stop("Restart & Install did not accept the accessibility action", 3)
            }
            return true
        }
        if named([card], button, ["Retry", "다시 시도", "Open Download Page", "다운로드 페이지 열기"]) != nil {
            stop("updater reported a failed download or manual installation")
        }
        if !downloadPressed, let download = named([card], button, ["Update Now", "지금 업데이트"]) {
            press(download, "Update Now"); downloadPressed = true
        }
        return false
    }
} else {
    let doc = args[3], version = args[4]
    var closedDocument = false, skippedWelcome = false, minimizedCard = false
    until(90, "the synthetic file row in Home > Recent") {
        if !minimizedCard, let card = updateCard(), let later = named([card], button, ["Minimize", "최소화"]) {
            press(later, "Minimize update"); minimizedCard = true; return false
        }
        let roots = windows()
        if !skippedWelcome, named(roots, text, ["Welcome to SeuOffice", "SeuOffice에 오신 것을 환영합니다"]) != nil,
           let skip = named(roots, button, ["Skip", "건너뛰기"]) {
            press(skip, "Skip onboarding"); skippedWelcome = true; return false
        }
        // Close the bridge's only document before updating. A restored tab must
        // not be able to repopulate Recent and mask a lost profile.
        if args[1] == "seed" {
            if let close = named(roots, button, ["Close tab", "탭 닫기"]) {
                if !closedDocument, named(roots, text, [doc]) != nil {
                    press(close, "Close synthetic document tab"); closedDocument = true
                }
                return false
            }
            if !closedDocument { return false }
        }
        if let recent = named(roots, ["AXGroup", "AXLandmarkRegion"], ["Recent", "최근 사용"]) {
            if named([recent], ["AXCheckBox"], ["Select " + doc, doc + " 선택"]) != nil ||
                named([recent], text, [doc]) != nil { return true }
        }
        // Some Chromium versions flatten the named region but retain the row checkbox.
        for window in roots where named([window], text, ["Recent", "최근 사용"]) != nil {
            if named([window], ["AXCheckBox"], ["Select " + doc, doc + " 선택"]) != nil { return true }
        }
        return false
    }
    click(["Settings", "설정"])
    click(["About", "정보"])
    until(30, "Settings > About exact version \(version)") {
        guard let dialog = named(windows(), ["AXGroup", "AXSheet", "AXDialog"], ["Settings", "설정"]) else { return false }
        return named([dialog], text, ["Version", "버전"]) != nil &&
            named([dialog], text, [version, "\(version) · Beta", "\(version) · 베타"]) != nil
    }
    click(["Cancel", "취소"])
}
