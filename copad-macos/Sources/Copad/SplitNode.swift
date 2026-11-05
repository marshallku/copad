import AppKit

enum SplitOrientation {
    case horizontal
    case vertical
}

indirect enum SplitNode {
    case leaf(any CopadPanel)
    case branch(SplitOrientation, [SplitNode])

    // MARK: - Leaf enumeration

    func allLeaves() -> [any CopadPanel] {
        switch self {
        case let .leaf(p): [p]
        case let .branch(_, children): children.flatMap { $0.allLeaves() }
        }
    }

    // MARK: - Tree mutations

    func splitting(
        _ panel: any CopadPanel,
        with newNode: SplitNode,
        orientation: SplitOrientation,
    ) -> SplitNode {
        switch self {
        case let .leaf(p):
            guard ObjectIdentifier(p) == ObjectIdentifier(panel) else { return self }
            return .branch(orientation, [.leaf(p), newNode])

        case let .branch(o, children):
            return .branch(o, children.map { $0.splitting(panel, with: newNode, orientation: orientation) })
        }
    }

    func removing(_ panel: any CopadPanel) -> SplitNode? {
        switch self {
        case let .leaf(p):
            return ObjectIdentifier(p) == ObjectIdentifier(panel) ? nil : self

        case let .branch(o, children):
            let remaining = children.compactMap { $0.removing(panel) }
            if remaining.isEmpty { return nil }
            if remaining.count == 1 { return remaining[0] }
            return .branch(o, remaining)
        }
    }
}
