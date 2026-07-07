import SpaceMeshCore
import SwiftUI

/// treemap 타일 하나. node가 nil이면 "현재 디렉토리 직속 파일" 잔여분 타일.
struct Tile: Identifiable {
    let id: Int
    let rect: CGRect
    let node: NodeInfo?
    let label: String
    let size: UInt64
    let color: Color
    /// 나이 오버레이용 서브트리 최신 mtime (0 = 미상).
    let newestMtime: Int64
}

/// 트리맵 색이 나타내는 것 — 크기(기본) 또는 나이 (F5).
enum TreemapOverlay: String, CaseIterable {
    case size = "크기"
    case age = "나이"
}

struct TreemapView: View {
    @EnvironmentObject var model: AppModel
    @State private var hoveredTileID: Int?
    @State private var overlay: TreemapOverlay = .size

    /// 화면에 그릴 최대 타일 수 — 그 이하는 "other"로 접는다 (LOD).
    private let maxTiles = 60

    var body: some View {
        GeometryReader { geo in
            let tiles = computeTiles(in: CGRect(origin: .zero, size: geo.size))
            Canvas { ctx, _ in
                for tile in tiles {
                    let r = tile.rect.insetBy(dx: 0.75, dy: 0.75)
                    guard r.width > 1, r.height > 1 else { continue }
                    let hovered = tile.id == hoveredTileID
                    ctx.fill(
                        Path(roundedRect: r, cornerRadius: 2),
                        with: .color(tile.color.opacity(hovered ? 1.0 : 0.88))
                    )
                    if hovered {
                        ctx.stroke(
                            Path(roundedRect: r.insetBy(dx: 0.75, dy: 0.75), cornerRadius: 2),
                            with: .color(Theme.accent),
                            lineWidth: 1.5
                        )
                    }
                    if r.width > 64, r.height > 30 {
                        let label = Text(tile.label)
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(Color(red: 0.95, green: 0.93, blue: 0.90))
                        ctx.draw(
                            label,
                            in: CGRect(
                                x: r.minX + 6, y: r.minY + 5,
                                width: r.width - 12, height: 14))
                        // 나이 모드에서는 크기 대신 "얼마나 오래됐는지"가 두 번째 줄.
                        let secondLine =
                            overlay == .age
                            ? humanAge(tile.newestMtime)
                            : humanBytes(tile.size)
                        let size = Text(secondLine)
                            .font(.system(size: 9.5, design: .monospaced))
                            .foregroundStyle(Color(red: 0.95, green: 0.93, blue: 0.90).opacity(0.75))
                        ctx.draw(
                            size,
                            in: CGRect(
                                x: r.minX + 6, y: r.minY + 19,
                                width: r.width - 12, height: 13))
                    }
                }
            }
            .gesture(
                SpatialTapGesture().onEnded { value in
                    guard
                        let tile = tiles.last(where: { $0.rect.contains(value.location) }),
                        let node = tile.node
                    else { return }
                    model.drill(into: node)
                }
            )
            .onContinuousHover { phase in
                switch phase {
                case .active(let location):
                    hoveredTileID = tiles.last(where: { $0.rect.contains(location) })?.id
                case .ended:
                    hoveredTileID = nil
                }
            }
            .overlay(alignment: .topTrailing) {
                overlayToggle
                    .padding(8)
            }
        }
        .background(Theme.bg)
    }

    /// 크기|나이 오버레이 전환 — 계기판 톤의 미니 세그먼트.
    private var overlayToggle: some View {
        HStack(spacing: 2) {
            ForEach(TreemapOverlay.allCases, id: \.self) { mode in
                Button {
                    overlay = mode
                } label: {
                    Text(mode.rawValue)
                        .font(.system(size: 10, weight: overlay == mode ? .bold : .medium))
                        .foregroundStyle(overlay == mode ? Theme.bg : Theme.textDim)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 3)
                        .background(
                            overlay == mode ? Theme.accent : .clear,
                            in: RoundedRectangle(cornerRadius: 4)
                        )
                }
                .buttonStyle(.plain)
            }
        }
        .padding(2)
        .background(Theme.panel.opacity(0.92), in: RoundedRectangle(cornerRadius: 6))
        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Theme.border, lineWidth: 1))
        .help("나이: 서브트리에서 가장 최근에 수정된 시점 기준 — 어두울수록 오래됨")
    }

    private func computeTiles(in rect: CGRect) -> [Tile] {
        guard let current = model.currentNode else { return [] }

        // 자식 디렉토리 + 직속 파일 잔여분을 하나의 값 목록으로.
        var items: [(node: NodeInfo?, label: String, size: UInt64)] =
            model.children.map { ($0, $0.name, $0.allocatedSize) }
        let childSum = model.children.reduce(UInt64(0)) { $0 + $1.allocatedSize }
        if current.allocatedSize > childSum {
            items.append((nil, "(files)", current.allocatedSize - childSum))
        }
        items.sort { $0.size > $1.size }

        // LOD: 상위 maxTiles만 그리고 나머지는 other로 합산.
        if items.count > maxTiles {
            let restSum = items[maxTiles...].reduce(UInt64(0)) { $0 + $1.size }
            items = Array(items[..<maxTiles])
            if restSum > 0 {
                items.append((nil, "(\(model.children.count - maxTiles) more)", restSum))
            }
        }
        items.removeAll { $0.size == 0 }
        guard !items.isEmpty else { return [] }

        let rects = Squarify.layout(values: items.map { CGFloat($0.size) }, in: rect)
        return zip(items.indices, zip(items, rects)).map { index, pair in
            let (item, tileRect) = pair
            let mtime = item.node?.newestMtime ?? 0
            let color: Color
            if item.node == nil {
                color = Theme.raised
            } else if overlay == .age {
                color = Self.color(for: item.label, dimming: Self.ageDimming(mtime))
            } else {
                color = Self.color(for: item.label)
            }
            return Tile(
                id: index,
                rect: tileRect,
                node: item.node,
                label: item.label,
                size: item.size,
                color: color,
                newestMtime: mtime
            )
        }
    }

    /// 나이 → 밝기 계수. 최근일수록 밝고 오래될수록 어둡다 (색상은 유지 — 원칙 2).
    static func ageDimming(_ mtime: Int64) -> Double {
        guard mtime > 0 else { return 0.55 }  // 미상은 중간 어둡기
        let days = Date().timeIntervalSince1970 - Double(mtime)
        switch days / 86_400 {
        case ..<30: return 1.0
        case ..<90: return 0.85
        case ..<365: return 0.68
        case ..<730: return 0.52
        default: return 0.38
        }
    }

    /// 이름에서 안정적인 색 — 같은 디렉토리는 항상 같은 색으로 보이게.
    /// 다크 배경 위에서 데이터가 주인공이 되도록 채도·명도를 계기판 톤으로 억제.
    /// dimming(나이 오버레이)은 명도만 낮춰 색 정체성을 유지한다.
    static func color(for name: String, dimming: Double = 1.0) -> Color {
        var hash: UInt64 = 5381
        for byte in name.utf8 {
            hash = hash &* 33 &+ UInt64(byte)
        }
        let hue = Double(hash % 360) / 360.0
        let saturation = 0.34 + Double((hash / 360) % 14) / 100.0  // 0.34~0.47
        let brightness = (0.50 + Double((hash / 5040) % 12) / 100.0) * dimming  // 0.50~0.61 × 나이
        return Color(hue: hue, saturation: saturation, brightness: brightness)
    }
}

/// unix 초 → 사람 눈에 맞춘 나이 문자열. 0 = 미상.
func humanAge(_ mtime: Int64) -> String {
    guard mtime > 0 else { return "—" }
    let days = (Date().timeIntervalSince1970 - Double(mtime)) / 86_400
    switch days {
    case ..<1: return "오늘"
    case ..<30: return "\(Int(days))일 전"
    case ..<365: return "\(Int(days / 30))개월 전"
    default: return String(format: "%.1f년 전", days / 365)
    }
}
