import AppKit
import SwiftUI

@main
struct SpaceMeshApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var model = AppModel()
    @ObservedObject private var settings = AppSettings.shared
    @ObservedObject private var agent = BackgroundAgent.shared

    init() {
        SelfTest.runIfRequested()
        // 저장된 모드를 시작 시 복원 (주기 LaunchAgent 재확인, 실시간 감시 재개).
        let s = AppSettings.shared
        BackgroundAgent.shared.apply(s)
    }

    var body: some Scene {
        WindowGroup("space-mesh") {
            ContentView()
                .environmentObject(model)
                .frame(minWidth: 900, minHeight: 560)
        }

        Settings {
            SettingsView()
        }

        // 실시간 모드일 때만 메뉴바에 상주 — 나머지 모드에선 노출하지 않는다.
        MenuBarExtra("space-mesh", systemImage: "internaldrive", isInserted: menuBarShown) {
            MenuBarContent()
        }
        .menuBarExtraStyle(.window)
    }

    private var menuBarShown: Binding<Bool> {
        .constant(settings.mode == .live)
    }
}

/// 메뉴바 팝오버 — 실시간 감시 상태 요약.
struct MenuBarContent: View {
    @ObservedObject var agent = BackgroundAgent.shared
    @ObservedObject var settings = AppSettings.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "internaldrive.fill").foregroundStyle(Theme.accent)
                Text("space-mesh").font(.system(size: 13, weight: .bold))
                Spacer()
                if agent.isRecomputing {
                    ProgressView().controlSize(.small)
                }
            }
            if let total = agent.lastTotal {
                HStack(spacing: 6) {
                    Text(humanBytes(total))
                        .font(.system(size: 22, weight: .semibold, design: .monospaced))
                        .foregroundStyle(Theme.text)
                    if settings.budgetGiB > 0 {
                        let budget = UInt64(settings.budgetGiB * 1_073_741_824)
                        Text("/ \(humanBytes(budget))")
                            .font(.dataCell)
                            .foregroundStyle(total > budget ? Theme.warn : Theme.textDim)
                    }
                }
            }
            Text(agent.status).font(.system(size: 11)).foregroundStyle(Theme.textDim)
            if let updated = agent.lastUpdated {
                Text("갱신 \(updated.formatted(date: .omitted, time: .shortened))")
                    .font(.system(size: 10)).foregroundStyle(Theme.textFaint)
            }
            Divider()
            Button("설정 열기…") {
                NSApp.activate(ignoringOtherApps: true)
                if #available(macOS 14, *) {
                    NSApp.sendAction(
                        Selector(("showSettingsWindow:")), to: nil, from: nil)
                }
            }
            Button("space-mesh 종료") { NSApp.terminate(nil) }
        }
        .padding(14)
        .frame(width: 260)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    /// 실시간 감시 중이면 창을 닫아도 메뉴바에 상주한다.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        AppSettings.shared.mode != .live
    }
}
