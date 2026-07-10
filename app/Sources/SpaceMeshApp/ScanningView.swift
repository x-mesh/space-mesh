import SpaceMeshCore
import SwiftUI

/// 스캔/해시 작업 중 화면 중앙의 레이더 스캔 뷰.
/// 광선이 회전하며 지나간 자리에 blip(발견된 파일 은유)이 반짝였다 사라진다.
/// Rust 코어의 scan_progress() 카운터를 폴링해 라이브로 파일 수를 보여준다.
struct ScanningView: View {
    let startedAt: Date
    var label = "스캔 중"
    var unit = "files"

    private let radarSize: CGFloat = 190
    private let sweepPeriod: Double = 2.4
    private let blipCount = 26

    /// 스캔 대기 시간에 순차로 보여주는 기능 힌트 — 설정에 숨은 기능의 발견 통로.
    private static let tips: [(title: String, body: String)] = [
        (
            "앱을 꺼도 감시할 수 있습니다",
            "우상단 ⚙︎ 설정에서 '주기 스냅샷'을 켜면 launchd가 조용히 스캔을 쌓아 '변화' 탭이 시간축으로 채워집니다. 상주 없음, 저전력 IO."
        ),
        (
            "디스크가 갑자기 불어나면 알림",
            "설정의 '실시간 감시' 모드는 급증(기본 5GiB)을 감지하면 주범 경로와 함께 macOS 알림을 보냅니다. 재집계는 변경분만 다시 읽어 1초 안에 끝납니다."
        ),
        (
            "크고 잊힌 파일 사냥",
            "회수 그룹의 '미수정 180일+' — 옛 dmg·ISO·백업처럼 오래 수정 없던 대용량 파일을 크기×경과일 순으로 랭킹합니다."
        ),
        (
            "뭐가 늘었는지 범인 추적",
            "'변화' 탭은 두 스냅샷을 비교해 증가·감소를 디렉토리별로 귀속합니다. 스캔할 때마다 스냅샷이 자동 저장됩니다."
        ),
        (
            "중복 정리는 클릭 한 번",
            "중복 검색 결과의 첫 파일은 최신 수정본(보존 추천)입니다. '추천본만 남기고 모두 선택' 버튼이면 정리 준비 끝."
        ),
        (
            "지우기 전에 Git부터",
            "안전 그룹의 Git 뷰는 미푸시 커밋·미커밋 변경이 있는 repo를 위험도로 표시합니다. 산출물 정리 전 확인용."
        ),
        (
            "터미널에서도 씁니다",
            "space-mesh ~ --ncdu > snap.json 후 ncdu -f snap.json. --dups --json, --categories --json으로 스크립팅도 됩니다."
        ),
        (
            "모든 삭제는 되돌릴 수 있습니다",
            "이 앱은 영구 삭제를 하지 않습니다 — 항상 휴지통 경유이고, 이동 직후엔 '되돌리기' 버튼도 있습니다."
        ),
    ]

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1.0 / 30.0)) { timeline in
            let elapsed = max(0, timeline.date.timeIntervalSince(startedAt))
            let count = scanProgress()
            let beamAngle = elapsed.truncatingRemainder(dividingBy: sweepPeriod)
                / sweepPeriod * 360.0

            VStack(spacing: 28) {
                Canvas { ctx, size in
                    let center = CGPoint(x: size.width / 2, y: size.height / 2)
                    let radius = min(size.width, size.height) / 2 - 4
                    drawGrid(ctx: &ctx, center: center, radius: radius)
                    drawSweep(ctx: &ctx, center: center, radius: radius, beamAngle: beamAngle)
                    drawBlips(
                        ctx: &ctx, center: center, radius: radius,
                        beamAngle: beamAngle, sweepIndex: Int(elapsed / sweepPeriod))
                }
                .frame(width: radarSize, height: radarSize)

                VStack(spacing: 8) {
                    Text(count.formatted())
                        .font(.readout)
                        .foregroundStyle(Theme.text)
                        .contentTransition(.numericText())
                    HStack(spacing: 8) {
                        InstrumentLabel(text: label)
                        Text("·").foregroundStyle(Theme.textFaint)
                        Text(String(format: "%.1fs", elapsed))
                            .font(.dataCell)
                            .foregroundStyle(Theme.textDim)
                        Text("·").foregroundStyle(Theme.textFaint)
                        InstrumentLabel(text: unit)
                    }
                }

                hintCard(elapsed: elapsed)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Theme.bg)
    }

    /// 8초마다 다음 힌트로 크로스페이드하는 기능 안내 카드.
    private func hintCard(elapsed: Double) -> some View {
        let idx = Int(elapsed / 8.0) % Self.tips.count
        let tip = Self.tips[idx]
        return VStack(spacing: 6) {
            InstrumentLabel(text: "TIP \(idx + 1)/\(Self.tips.count)")
            Text(tip.title)
                .font(.system(size: 12.5, weight: .semibold))
                .foregroundStyle(Theme.text)
            Text(tip.body)
                .font(.system(size: 11.5))
                .foregroundStyle(Theme.textDim)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .lineSpacing(2)
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 12)
        .frame(maxWidth: 460)
        .background(Theme.panel, in: RoundedRectangle(cornerRadius: 8))
        .id(idx)
        .transition(.opacity)
        .animation(.easeOut(duration: 0.5), value: idx)
    }

    // MARK: - 레이더 요소

    /// 동심원 + 십자선 그리드.
    private func drawGrid(ctx: inout GraphicsContext, center: CGPoint, radius: CGFloat) {
        for ring in [1.0, 0.66, 0.33] {
            let r = radius * ring
            ctx.stroke(
                Path(ellipseIn: CGRect(
                    x: center.x - r, y: center.y - r, width: r * 2, height: r * 2)),
                with: .color(Theme.border),
                lineWidth: 1
            )
        }
        var cross = Path()
        cross.move(to: CGPoint(x: center.x - radius, y: center.y))
        cross.addLine(to: CGPoint(x: center.x + radius, y: center.y))
        cross.move(to: CGPoint(x: center.x, y: center.y - radius))
        cross.addLine(to: CGPoint(x: center.x, y: center.y + radius))
        ctx.stroke(cross, with: .color(Theme.border.opacity(0.6)), lineWidth: 1)
        // 중심점
        ctx.fill(
            Path(ellipseIn: CGRect(x: center.x - 2, y: center.y - 2, width: 4, height: 4)),
            with: .color(Theme.textDim))
    }

    /// 회전 광선 — 진행 방향으로 밝고 뒤로 갈수록 잦아드는 부채꼴 잔광.
    private func drawSweep(
        ctx: inout GraphicsContext, center: CGPoint, radius: CGFloat, beamAngle: Double
    ) {
        let beamRad = CGFloat(Angle(degrees: beamAngle).radians)
        // 잔광 부채꼴 (여러 겹으로 페이드)
        let trailSteps = 24
        let trailSpan = 70.0
        for step in 0..<trailSteps {
            let a0 = beamAngle - trailSpan * Double(step + 1) / Double(trailSteps)
            let a1 = beamAngle - trailSpan * Double(step) / Double(trailSteps)
            var wedge = Path()
            wedge.move(to: center)
            wedge.addArc(
                center: center, radius: radius,
                startAngle: .degrees(a0), endAngle: .degrees(a1), clockwise: false)
            wedge.closeSubpath()
            let alpha = 0.16 * (1.0 - Double(step) / Double(trailSteps))
            ctx.fill(wedge, with: .color(Theme.accent.opacity(alpha)))
        }
        // 광선 본체
        var beam = Path()
        beam.move(to: center)
        beam.addLine(to: CGPoint(
            x: center.x + cos(beamRad) * radius,
            y: center.y + sin(beamRad) * radius))
        ctx.stroke(beam, with: .color(Theme.accent.opacity(0.9)), lineWidth: 1.5)
    }

    /// blip — 광선이 지나간 자리에 나타났다가 서서히 사라지는 점.
    /// 위치는 (blip index, 회전 바퀴수) 해시로 결정해 바퀴마다 다른 곳에서 반짝인다.
    private func drawBlips(
        ctx: inout GraphicsContext, center: CGPoint, radius: CGFloat,
        beamAngle: Double, sweepIndex: Int
    ) {
        for i in 0..<blipCount {
            // 현재 바퀴와 직전 바퀴의 blip을 함께 그려 경계에서 끊기지 않게 한다.
            for sweep in [sweepIndex, sweepIndex - 1] {
                guard sweep >= 0 else { continue }
                var h = UInt64(i) &* 2654435761 &+ UInt64(sweep) &* 40503
                h ^= h >> 13
                h = h &* 97 &+ 31
                let angle = Double(h % 360)
                let dist = 0.18 + Double((h / 360) % 75) / 100.0  // 0.18~0.93 반경
                // 이 바퀴에서 광선이 blip 각도를 지났는지, 지난 후 얼마나 됐는지.
                let sweepsAngle = Double(sweepIndex - sweep) * 360.0 + beamAngle
                let since = sweepsAngle - angle
                guard since > 0, since < 300 else { continue }
                let alpha = max(0, 1.0 - since / 300.0)
                let rad = Angle(degrees: angle).radians
                let pos = CGPoint(
                    x: center.x + cos(rad) * radius * dist,
                    y: center.y + sin(rad) * radius * dist)
                let blipSize: CGFloat = since < 25 ? 5 : 3.5
                ctx.fill(
                    Path(ellipseIn: CGRect(
                        x: pos.x - blipSize / 2, y: pos.y - blipSize / 2,
                        width: blipSize, height: blipSize)),
                    with: .color(Theme.accent.opacity(alpha * 0.85)))
            }
        }
    }
}
