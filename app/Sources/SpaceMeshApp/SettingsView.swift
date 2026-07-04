import AppKit
import SwiftUI

/// 백그라운드 감시 설정 — 세 모드 중 택일 + 세부 옵션.
struct SettingsView: View {
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject var agent = BackgroundAgent.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("백그라운드 감시")
                .font(.system(size: 15, weight: .bold))
                .foregroundStyle(Theme.text)

            // 모드 3-way 카드 선택
            VStack(spacing: 8) {
                ForEach(WatchMode.allCases) { mode in
                    modeCard(mode)
                }
            }

            // 모드별 세부 옵션
            if settings.mode == .periodic {
                HStack(spacing: 10) {
                    InstrumentLabel(text: "간격")
                    Picker("", selection: $settings.interval) {
                        ForEach(PeriodicInterval.allCases) { iv in
                            Text(iv.title).tag(iv)
                        }
                    }
                    .labelsHidden()
                    .frame(width: 130)
                }
                .padding(.leading, 4)
            }
            if settings.mode == .live {
                HStack(spacing: 10) {
                    InstrumentLabel(text: "예산")
                    TextField("GiB", value: $settings.budgetGiB, format: .number)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 70)
                    Text("GiB 초과 시 알림 (0 = 끔)")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textDim)
                }
                .padding(.leading, 4)
                HStack(spacing: 10) {
                    InstrumentLabel(text: "급증")
                    TextField("GiB", value: $settings.growthAlertGiB, format: .number)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 70)
                    Text("직전 스냅샷 대비 이만큼 늘면 주범과 함께 알림 (0 = 끔)")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textDim)
                }
                .padding(.leading, 4)
            }

            // 감시 대상
            if settings.mode != .off {
                HStack(spacing: 8) {
                    InstrumentLabel(text: "대상")
                    Text(settings.watchedRoot)
                        .font(.pathCell)
                        .foregroundStyle(Theme.textDim)
                        .lineLimit(1)
                        .truncationMode(.head)
                    Button("변경") {
                        let panel = NSOpenPanel()
                        panel.canChooseDirectories = true
                        panel.canChooseFiles = false
                        if panel.runModal() == .OK, let url = panel.url {
                            settings.watchedRoot = url.path
                            agent.apply(settings)
                        }
                    }
                    .font(.system(size: 11))
                }
                .padding(.leading, 4)
            }

            Divider().overlay(Theme.border)

            HStack(spacing: 6) {
                if agent.isRecomputing {
                    ProgressView().controlSize(.small)
                }
                Text(agent.status.isEmpty ? "대기 중" : agent.status)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textDim)
            }

            Spacer()
        }
        .padding(20)
        .frame(width: 460, height: 420)
        .background(Theme.bg)
        .onChange(of: settings.mode) { agent.apply(settings) }
        .onChange(of: settings.interval) {
            if settings.mode == .periodic { agent.apply(settings) }
        }
        .onChange(of: settings.budgetGiB) {
            if settings.mode == .live { agent.apply(settings) }
        }
        .onChange(of: settings.growthAlertGiB) {
            if settings.mode == .live { agent.apply(settings) }
        }
    }

    private func modeCard(_ mode: WatchMode) -> some View {
        let selected = settings.mode == mode
        return Button {
            settings.mode = mode
        } label: {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: selected ? "largecircle.fill.circle" : "circle")
                    .font(.system(size: 14))
                    .foregroundStyle(selected ? Theme.accent : Theme.textFaint)
                VStack(alignment: .leading, spacing: 3) {
                    Text(mode.title)
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    Text(mode.summary)
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textDim)
                        .fixedSize(horizontal: false, vertical: true)
                        .multilineTextAlignment(.leading)
                }
                Spacer()
            }
            .padding(12)
            .background(
                RoundedRectangle(cornerRadius: 8)
                    .fill(selected ? Theme.accentSoft : Theme.panel)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .stroke(selected ? Theme.accent.opacity(0.5) : Theme.border, lineWidth: 1)
            )
        }
        .buttonStyle(.plain)
    }
}
