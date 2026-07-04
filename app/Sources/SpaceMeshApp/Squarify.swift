import CoreGraphics

/// Bruls squarified treemap 레이아웃.
/// 입력 values는 내림차순이어야 종횡비가 최적에 가깝다. 반환 rect 순서는 입력 순서와 같다.
enum Squarify {
    static func layout(values: [CGFloat], in rect: CGRect) -> [CGRect] {
        let total = values.reduce(0, +)
        guard total > 0, rect.width > 0, rect.height > 0 else {
            return values.map { _ in .zero }
        }
        let scale = rect.width * rect.height / total
        let areas = values.map { $0 * scale }

        var result: [CGRect] = []
        result.reserveCapacity(areas.count)
        var remaining = rect
        var i = 0
        while i < areas.count {
            let side = min(remaining.width, remaining.height)
            var row: [CGFloat] = [areas[i]]
            var j = i + 1
            // 다음 항목을 추가해도 최악 종횡비가 나빠지지 않는 동안 행을 늘린다.
            while j < areas.count {
                if worstAspect(row + [areas[j]], side: side) <= worstAspect(row, side: side) {
                    row.append(areas[j])
                    j += 1
                } else {
                    break
                }
            }

            let rowArea = row.reduce(0, +)
            let thickness = side > 0 ? rowArea / side : 0
            var offset: CGFloat = 0
            let horizontal = remaining.width >= remaining.height
            for area in row {
                let length = thickness > 0 ? area / thickness : 0
                if horizontal {
                    // 왼쪽에 세로 스트립: 폭 = thickness, 세로로 나눠 담는다.
                    result.append(CGRect(
                        x: remaining.minX, y: remaining.minY + offset,
                        width: thickness, height: length
                    ))
                } else {
                    result.append(CGRect(
                        x: remaining.minX + offset, y: remaining.minY,
                        width: length, height: thickness
                    ))
                }
                offset += length
            }
            if horizontal {
                remaining = CGRect(
                    x: remaining.minX + thickness, y: remaining.minY,
                    width: remaining.width - thickness, height: remaining.height
                )
            } else {
                remaining = CGRect(
                    x: remaining.minX, y: remaining.minY + thickness,
                    width: remaining.width, height: remaining.height - thickness
                )
            }
            i = j
        }
        return result
    }

    private static func worstAspect(_ row: [CGFloat], side: CGFloat) -> CGFloat {
        let sum = row.reduce(0, +)
        guard sum > 0, side > 0, let maxA = row.max(), let minA = row.min(), minA > 0 else {
            return .infinity
        }
        let s2 = sum * sum
        let w2 = side * side
        return max(w2 * maxA / s2, s2 / (w2 * minA))
    }
}
