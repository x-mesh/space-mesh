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
}

struct TreemapView: View {
    @EnvironmentObject var model: AppModel
    @State private var hoveredTileID: Int?

    /// 화면에 그릴 최대 타일 수 — 그 이하는 "other"로 접는다 (LOD).
    private let maxTiles = 60

    var body: some View {
        GeometryReader { geo in
            let tiles = computeTiles(in: CGRect(origin: .zero, size: geo.size))
            Canvas { ctx, _ in
                for tile in tiles {
                    let r = tile.rect.insetBy(dx: 1, dy: 1)
                    guard r.width > 1, r.height > 1 else { continue }
                    let hovered = tile.id == hoveredTileID
                    ctx.fill(
                        Path(roundedRect: r, cornerRadius: 2),
                        with: .color(tile.color.opacity(hovered ? 1.0 : 0.94))
                    )
                    ctx.stroke(
                        Path(roundedRect: r.insetBy(dx: 0.5, dy: 0.5), cornerRadius: 2),
                        with: .color(hovered ? Theme.accent : Color.white.opacity(0.16)),
                        lineWidth: hovered ? 1.5 : 1
                    )
                    if r.width > 64, r.height > 30 {
                        let label = Text(tile.label)
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(Theme.text)
                        ctx.draw(
                            label,
                            in: CGRect(
                                x: r.minX + 6, y: r.minY + 5,
                                width: r.width - 12, height: 14))
                        let size = Text(humanBytes(tile.size))
                            .font(.system(size: 9.5, design: .monospaced))
                            .foregroundStyle(Theme.text.opacity(0.78))
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
        }
        .background(Theme.bg)
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
            return Tile(
                id: index,
                rect: tileRect,
                node: item.node,
                label: item.label,
                size: item.size,
                color: item.node == nil
                    ? Theme.raised
                    : Self.color(for: item.label)
            )
        }
    }

    /// 이름에서 안정적인 색 — 같은 디렉토리는 항상 같은 색으로 보이게.
    /// 다크 배경 위에서 데이터가 주인공이 되도록 채도·명도를 계기판 톤으로 억제.
    static func color(for name: String) -> Color {
        var hash: UInt64 = 5381
        for byte in name.utf8 {
            hash = hash &* 33 &+ UInt64(byte)
        }
        // 달 표면과 어울리는 저채도 광물색. 색상과 명도를 함께 분산해 인접 타일을 구분한다.
        let palette: [Color] = [
            Color(red: 0.25, green: 0.31, blue: 0.33), // lunar teal
            Color(red: 0.31, green: 0.32, blue: 0.36), // blue gray
            Color(red: 0.34, green: 0.31, blue: 0.35), // mineral violet
            Color(red: 0.36, green: 0.34, blue: 0.30), // muted bronze
            Color(red: 0.29, green: 0.34, blue: 0.31), // lunar sage
            Color(red: 0.38, green: 0.38, blue: 0.39), // moon stone
        ]
        return palette[Int(hash % UInt64(palette.count))]
    }
}
