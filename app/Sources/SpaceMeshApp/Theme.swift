import SwiftUI

/// space-mesh 디자인 토큰 — "정밀 계기판" 다크 테마.
/// 표면은 앰버 쪽으로 틴트된 웜 다크 뉴트럴(순수 검정 금지), 악센트는 웜 앰버 하나.
/// 컬러는 데이터(treemap)가 담당하고 UI 크롬은 모노톤을 유지한다.
enum Theme {
    // 표면 — 거의 중성, 눈치채기 어려운 정도의 온기만 남긴다.
    static let bg = Color(red: 0.089, green: 0.088, blue: 0.086)
    static let panel = Color(red: 0.122, green: 0.120, blue: 0.117)
    static let raised = Color(red: 0.159, green: 0.157, blue: 0.153)
    static let border = Color(red: 0.29, green: 0.285, blue: 0.275).opacity(0.45)

    // 텍스트 — 중성 그레이 스케일.
    static let text = Color(red: 0.905, green: 0.900, blue: 0.890)
    static let textDim = Color(red: 0.605, green: 0.600, blue: 0.585)
    static let textFaint = Color(red: 0.435, green: 0.430, blue: 0.418)

    // 악센트: 노란기를 뺀 코퍼(구리) 톤 — 강조 지점에만 절제해서 사용.
    static let accent = Color(red: 0.870, green: 0.560, blue: 0.330)
    static let accentSoft = Color(red: 0.870, green: 0.560, blue: 0.330).opacity(0.15)

    // 시맨틱
    static let safe = Color(red: 0.478, green: 0.729, blue: 0.462)
    static let warn = Color(red: 0.886, green: 0.557, blue: 0.286)
    static let info = Color(red: 0.475, green: 0.639, blue: 0.827)

    // 발산형 차트 페어 (diff 시각화) — dataviz validator 통과값.
    // 다크 표면 #171616에서 L 밴드/크로마/CVD(protan ΔE 42.2)/대비 전부 PASS.
    static let deltaGrow = Color(red: 0xC7 / 255.0, green: 0x7E / 255.0, blue: 0x42 / 255.0)
    static let deltaShrink = Color(red: 0x2B / 255.0, green: 0xA3 / 255.0, blue: 0x9B / 255.0)
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
