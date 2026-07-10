import AppKit
import SwiftUI

/// space-mesh 디자인 토큰 — 심우주 관제 계기판 테마.
/// 컬러는 데이터(treemap)가 담당하고 UI 크롬은 모노톤을 유지한다.
enum Theme {
    // 표면 — 거의 중성, 눈치채기 어려운 정도의 온기만 남긴다.
    static let bg = Color(red: 0.018, green: 0.018, blue: 0.020)
    static let panel = Color(red: 0.035, green: 0.036, blue: 0.040)
    static let raised = Color(red: 0.075, green: 0.076, blue: 0.080)
    static let border = Color(red: 0.52, green: 0.52, blue: 0.54).opacity(0.42)

    // 텍스트 — 중성 그레이 스케일.
    static let text = Color(red: 0.94, green: 0.94, blue: 0.94)
    static let textDim = Color(red: 0.62, green: 0.62, blue: 0.64)
    static let textFaint = Color(red: 0.40, green: 0.40, blue: 0.42)

    // 악센트: 노란기를 뺀 코퍼(구리) 톤 — 강조 지점에만 절제해서 사용.
    static let accent = Color(red: 0.46, green: 0.72, blue: 0.73)
    static let accentSoft = Color(red: 0.46, green: 0.72, blue: 0.73).opacity(0.12)

    // 시맨틱
    static let safe = Color(red: 0.478, green: 0.729, blue: 0.462)
    static let warn = Color(red: 0.95, green: 0.61, blue: 0.32)
    static let info = Color(red: 0.42, green: 0.65, blue: 0.95)

    // 발산형 차트 페어 (diff 시각화) — dataviz validator 통과값.
    // 다크 표면 #171616에서 L 밴드/크로마/CVD(protan ΔE 42.2)/대비 전부 PASS.
    static let deltaGrow = Color(red: 0xC7 / 255.0, green: 0x7E / 255.0, blue: 0x42 / 255.0)
    static let deltaShrink = Color(red: 0x2B / 255.0, green: 0xA3 / 255.0, blue: 0x9B / 255.0)
}

/// 정적인 별자리 그리드. 빈 상태에서만 사용해 데이터 화면의 집중도를 유지한다.
struct SpaceGrid: View {
    var body: some View {
        Canvas { context, size in
            let points: [(CGFloat, CGFloat, CGFloat)] = [
                (0.12, 0.18, 1.5), (0.28, 0.36, 1), (0.46, 0.16, 1),
                (0.63, 0.30, 1.5), (0.82, 0.20, 1), (0.74, 0.58, 1.2),
                (0.18, 0.72, 1), (0.42, 0.82, 1.2), (0.90, 0.78, 1)
            ]
            for (x, y, radius) in points {
                let rect = CGRect(x: size.width * x - radius, y: size.height * y - radius,
                                  width: radius * 2, height: radius * 2)
                context.fill(Path(ellipseIn: rect), with: .color(Theme.accent.opacity(0.5)))
            }
            var grid = Path()
            grid.move(to: CGPoint(x: 0, y: size.height * 0.26))
            grid.addLine(to: CGPoint(x: size.width, y: size.height * 0.26))
            grid.move(to: CGPoint(x: size.width * 0.64, y: 0))
            grid.addLine(to: CGPoint(x: size.width * 0.64, y: size.height))
            context.stroke(grid, with: .color(Theme.border.opacity(0.28)), lineWidth: 1)
        }
        .allowsHitTesting(false)
    }
}

/// 앱 bundle의 표준 Resources 경로에서 달 이미지를 읽는다.
struct MoonBackdrop: View {
    var body: some View {
        if let url = Bundle.main.url(forResource: "MoonBackground", withExtension: "png"),
           let image = NSImage(contentsOf: url)
        {
            Image(nsImage: image)
                .resizable()
                .scaledToFill()
        } else {
            Theme.bg
        }
    }
}

extension Font {
    /// 계기판 섹션 라벨 — 대문자 + .tracking(1.2)와 함께 사용.
    static let instrumentLabel = Font.system(size: 10.5, weight: .semibold)
    /// 큰 숫자 리드아웃 (스캔 카운터 등).
    static let readout = Font.system(size: 30, weight: .semibold, design: .monospaced)
    /// 표 데이터 셀 (크기·개수).
    static let dataCell = Font.system(size: 12, weight: .medium, design: .monospaced)
    /// 경로 표시.
    static let pathCell = Font.system(size: 10.5, design: .monospaced)
}

/// 상태 태그 (안전/검토 필요/확인됨 등) — 공용 캡슐 배지.
struct TagBadge: View {
    let text: String
    let color: Color
    var icon: String? = nil

    var body: some View {
        HStack(spacing: 3) {
            if let icon {
                Image(systemName: icon).font(.system(size: 8, weight: .bold))
            }
            Text(text).font(.system(size: 9.5, weight: .semibold)).tracking(0.4)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(color.opacity(0.16), in: Capsule())
        .foregroundStyle(color)
    }
}

/// 계기판 스타일 대문자 섹션 라벨.
struct InstrumentLabel: View {
    let text: String
    var body: some View {
        Text(text.uppercased())
            .font(.instrumentLabel)
            .tracking(1.4)
            .foregroundStyle(Theme.textFaint)
    }
}
