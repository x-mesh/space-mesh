import AppKit
import SpaceMeshCore
import SwiftUI

/// git repo 건강도 — 스캔 트리에서 발견한 repo를 위험도순으로.
/// 모든 remote 판정은 로컬 캐시 기준(fetch 안 함) — UI에 명시.
struct GitView: View {
    @EnvironmentObject var appModel: AppModel
    let scanTarget: String

    @State private var report: GitReport?
    @State private var isLoading = false
    @State private var loadedRoot = ""
    @State private var filter: RiskFilter = .all
    @State private var activityCache: [String: [UInt64]] = [:]

    enum RiskFilter: String, CaseIterable {
        case all = "전체"
        case danger = "위험"
        case caution = "주의"
        case abandoned = "방치"
        case failures = "조회실패"
    }

    private var repos: [GitRepoInfo] { report?.repos ?? [] }

    private var filtered: [GitRepoInfo] {
        switch filter {
        case .all: return repos
        case .danger: return repos.filter { $0.risk == "danger" }
        case .caution: return repos.filter { $0.risk == "caution" }
        case .abandoned: return repos.filter { $0.risk == "abandoned" }
        case .failures: return []
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.border).frame(height: 1)
            content
        }
        .onAppear { reloadIfNeeded() }
        .onChange(of: appModel.scanSeconds) { reloadIfNeeded(force: true) }
    }

    // MARK: - Header

    private var header: some View {
        HStack(spacing: 12) {
            if let r = report {
                summaryChip("전체", count: r.repos.count, color: Theme.textDim)
                summaryChip(
                    "위험", count: r.repos.filter { $0.risk == "danger" }.count, color: Theme.warn)
                summaryChip(
                    "방치", count: r.repos.filter { $0.risk == "abandoned" }.count,
                    color: Theme.textFaint)
                if !r.failures.isEmpty {
                    summaryChip("조회실패", count: r.failures.count, color: Theme.deltaShrink)
                }
                let stale = r.repos.filter { ($0.remoteStaleDays ?? 0) > 30 }.count
                if stale > 0 {
                    summaryChip("원격정보 오래됨", count: stale, color: Theme.info)
                }
            }
            Spacer()
            Picker("", selection: $filter) {
                ForEach(RiskFilter.allCases, id: \.self) { f in
                    Text(f.rawValue).tag(f)
                }
            }
            .pickerStyle(.segmented)
            .frame(width: 320)
            Button {
                reloadIfNeeded(force: true)
            } label: {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.textDim)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }

    private func summaryChip(_ label: String, count: Int, color: Color) -> some View {
        HStack(spacing: 5) {
            Text("\(count)").font(.dataCell).foregroundStyle(color)
            InstrumentLabel(text: label)
        }
    }

    // MARK: - Content

    @ViewBuilder
    private var content: some View {
        if appModel.handle == nil {
            emptyState(
                icon: "point.3.connected.trianglepath.dotted",
                title: "repo 분석은 스캔 결과를 사용합니다",
                sub: "'\((scanTarget as NSString).lastPathComponent)'를 스캔하면 그 안의 git repo를 찾습니다",
                showScan: true)
        } else if isLoading {
            VStack(spacing: 10) {
                ProgressView().controlSize(.small)
                Text("git 상태 조회 중… (로컬 캐시 기준, 네트워크 없음)")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textDim)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if filter == .failures {
            failureList
        } else if filtered.isEmpty {
            emptyState(
                icon: "checkmark.seal",
                title: repos.isEmpty ? "스캔 범위에 git repo가 없습니다" : "이 필터에 해당하는 repo가 없습니다",
                sub: "", showScan: false)
        } else {
            repoList
        }
    }

    private var repoList: some View {
        List {
            ForEach(filtered, id: \.path) { repo in
                repoRow(repo)
                    .listRowBackground(Theme.bg)
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func repoRow(_ repo: GitRepoInfo) -> some View {
        HStack(spacing: 10) {
            // 위험도 점
            Circle()
                .fill(riskColor(repo.risk))
                .frame(width: 9, height: 9)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text((repo.path as NSString).lastPathComponent)
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    branchLabel(repo.head)
                }
                badges(repo)
                Text(repo.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            Spacer()
            sparkline(repo)
            VStack(alignment: .trailing, spacing: 1) {
                if let days = repo.lastCommitDays {
                    Text(days == 0 ? "오늘" : "\(days)일 전")
                        .font(.dataCell)
                        .foregroundStyle(days > 180 ? Theme.textFaint : Theme.textDim)
                } else {
                    Text("커밋 없음").font(.dataCell).foregroundStyle(Theme.warn)
                }
            }
        }
        .padding(.vertical, 3)
        .contentShape(Rectangle())
        .onTapGesture {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: repo.path)])
        }
        .contextMenu {
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: repo.path)])
            }
            Button("터미널에서 열기") {
                let script = "tell application \"Terminal\" to do script \"cd '\(repo.path)'\""
                if let apple = NSAppleScript(source: script) {
                    apple.executeAndReturnError(nil)
                }
            }
        }
        .onAppear { loadActivity(repo.path) }
    }

    // MARK: - Badges & sparkline

    @ViewBuilder
    private func badges(_ repo: GitRepoInfo) -> some View {
        HStack(spacing: 4) {
            if repo.trackedDirty > 0 {
                TagBadge(text: "미커밋 \(repo.trackedDirty)", color: Theme.warn)
            }
            if repo.untrackedPresent {
                TagBadge(text: "untracked", color: Theme.warn)
            }
            if repo.stashCount > 0 {
                TagBadge(text: "stash \(repo.stashCount)", color: Theme.warn)
            }
            if repo.ahead > 0 {
                TagBadge(text: "미푸시 \(repo.ahead)", color: Theme.deltaGrow)
                    .help("현재 브랜치 upstream 대비 ahead \(repo.ahead) (로컬 캐시 기준)")
            }
            if !repo.hasRemote {
                TagBadge(text: "remote 없음", color: Theme.deltaShrink)
            }
            if repo.noUpstream && repo.hasRemote {
                TagBadge(text: "upstream 없음", color: Theme.textFaint)
                    .help("현재 브랜치에 추적 upstream 미설정 — 로컬 전용")
            }
            if repo.head == "detached" {
                TagBadge(text: "detached", color: Theme.warn)
            }
            if let stale = repo.remoteStaleDays, stale > 30 {
                TagBadge(text: "원격정보 \(stale)일 전", color: Theme.info)
                    .help("마지막 fetch가 오래됨 — 미푸시 판정이 부정확할 수 있음")
            }
        }
    }

    private func sparkline(_ repo: GitRepoInfo) -> some View {
        let data = activityCache[repo.path] ?? []
        let maxV = data.max() ?? 1
        return HStack(alignment: .bottom, spacing: 1.5) {
            ForEach(Array(data.enumerated()), id: \.offset) { _, v in
                RoundedRectangle(cornerRadius: 0.5)
                    .fill(v > 0 ? Theme.accent.opacity(0.8) : Theme.border)
                    .frame(
                        width: 2.5,
                        height: max(1.5, CGFloat(v) / CGFloat(max(maxV, 1)) * 18))
            }
        }
        .frame(height: 18)
        .help("최근 12주 주별 커밋")
    }

    private func branchLabel(_ head: String) -> some View {
        let name: String
        if head.hasPrefix("branch:") {
            name = String(head.dropFirst("branch:".count))
        } else if head == "unborn" {
            name = "커밋 없음"
        } else {
            name = head
        }
        return HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.branch").font(.system(size: 9))
            Text(name).font(.system(size: 10, design: .monospaced))
        }
        .foregroundStyle(Theme.textFaint)
    }

    private var failureList: some View {
        List {
            ForEach(report?.failures ?? [], id: \.path) { f in
                HStack(spacing: 10) {
                    Image(systemName: "exclamationmark.triangle")
                        .foregroundStyle(Theme.warn)
                    VStack(alignment: .leading, spacing: 1) {
                        Text((f.path as NSString).lastPathComponent)
                            .font(.system(size: 12))
                            .foregroundStyle(Theme.text)
                        Text(failureHint(f.reason))
                            .font(.system(size: 10.5))
                            .foregroundStyle(Theme.textDim)
                    }
                    Spacer()
                }
                .listRowBackground(Theme.bg)
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func failureHint(_ reason: String) -> String {
        switch reason {
        case "git_missing": return "git이 설치되어 있지 않습니다 — Xcode Command Line Tools 설치 필요"
        case "timeout": return "조회 시간 초과 — 대형 repo이거나 디스크가 바쁩니다"
        case "permission_denied": return "접근 권한 없음 — 전체 디스크 접근 필요할 수 있음"
        case "not_a_repo": return "유효한 git repo가 아닙니다"
        case "corrupted": return "repo가 손상되었습니다"
        default: return reason
        }
    }

    // MARK: - Helpers

    private func riskColor(_ risk: String) -> Color {
        switch risk {
        case "danger": return Theme.warn
        case "caution": return Theme.deltaGrow
        case "abandoned": return Theme.textFaint
        case "active": return Theme.safe
        default: return Theme.info
        }
    }

    private func emptyState(icon: String, title: String, sub: String, showScan: Bool) -> some View {
        VStack(spacing: 12) {
            Image(systemName: icon)
                .font(.system(size: 40, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text(title).font(.title3.weight(.semibold)).foregroundStyle(Theme.text)
            if !sub.isEmpty {
                Text(sub).font(.callout).foregroundStyle(Theme.textDim)
            }
            if showScan {
                Button("'\((scanTarget as NSString).lastPathComponent)' 스캔") {
                    appModel.startScan(path: scanTarget)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func reloadIfNeeded(force: Bool = false) {
        guard appModel.handle != nil else { return }
        if force || loadedRoot != appModel.scannedRoot || report == nil {
            reload()
        }
    }

    private func reload() {
        guard let handle = appModel.handle else { return }
        isLoading = true
        activityCache = [:]
        Task {
            let r = await Task.detached(priority: .userInitiated) {
                // 캐시 경유 — 변경 없는 repo는 git 프로세스를 띄우지 않는다.
                handle.gitReposCached(includeSubmodules: false, dbPath: AppModel.dbPath)
            }.value
            self.report = r
            self.loadedRoot = appModel.scannedRoot
            self.isLoading = false
        }
    }

    /// 스파크라인은 행이 보일 때 lazy 로드 (git log 비용 회피).
    private func loadActivity(_ path: String) {
        guard activityCache[path] == nil else { return }
        activityCache[path] = []  // 중복 로드 방지
        Task {
            let data = await Task.detached(priority: .background) {
                gitActivity(repoPath: path, weeks: 12)
            }.value
            self.activityCache[path] = data
        }
    }
}
