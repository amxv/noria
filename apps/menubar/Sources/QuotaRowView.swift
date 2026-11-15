import AppKit

// Remaining-allowance level: green normally, yellow below 25% left, and red
// below 10% left. Kept free of AppKit drawing so validators can check it.
enum QuotaLevel: Equatable {
    case healthy, low, critical
    init(percentLeft: Double) {
        if percentLeft < 10 { self = .critical } else if percentLeft < 25 { self = .low } else { self = .healthy }
    }
    var color: NSColor {
        switch self {
        case .healthy: return .systemGreen
        case .low: return .systemYellow
        case .critical: return .systemRed
        }
    }
}

struct QuotaRowModel {
    let title: String
    let percentLeft: Double?   // 0...100 share of the allowance remaining
    let percentText: String
    let resetText: String
    let resetClock: String?

    init(title: String, window: QuotaWindow, now: Date = Date()) {
        self.title = title
        if let used = window.used_percent, used.isFinite {
            let left = max(0, min(100, 100 - used))
            percentLeft = left
            percentText = String(format: "%.0f%% left", left)
        } else {
            percentLeft = nil
            percentText = "Unknown"
        }
        guard let reset = window.resets_at else {
            resetText = "Reset unknown"; resetClock = nil
            return
        }
        let seconds = reset.timeIntervalSince(now)
        resetText = seconds <= 0 ? "Refreshing reset" : "Resets in \(quotaCountdown(seconds))"
        let format = DateFormatter()
        format.locale = .current
        format.setLocalizedDateFormatFromTemplate(seconds >= 86_400 ? "EEE j:mm" : "j:mm")
        resetClock = format.string(from: reset)
    }

    var level: QuotaLevel? { percentLeft.map(QuotaLevel.init(percentLeft:)) }
}

func quotaCountdown(_ seconds: TimeInterval) -> String {
    let minutes = Int(ceil(seconds / 60))
    if minutes >= 1440 { return "\(minutes / 1440)d \((minutes % 1440) / 60)h" }
    if minutes >= 60 { return "\(minutes / 60)h \(minutes % 60)m" }
    return "\(minutes)m"
}

// A custom menu row: bold limit name and remaining percentage, a coloured
// bar for the allowance left, and a bold reset countdown with its clock time.
final class QuotaRowView: NSView {
    static let height: CGFloat = 58
    private let model: QuotaRowModel
    private let inset: CGFloat = 32
    private let trailing: CGFloat = 16

    init(model: QuotaRowModel, width: CGFloat = 300) {
        self.model = model
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: QuotaRowView.height))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
        var spoken = "\(model.title), \(model.percentText), \(model.resetText)"
        if let clock = model.resetClock { spoken += " at \(clock)" }
        setAccessibilityLabel(spoken)
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unused") }
    override var isFlipped: Bool { true }
    override var allowsVibrancy: Bool { false }

    override func draw(_ dirtyRect: NSRect) {
        let left = inset, right = bounds.width - trailing, width = right - left
        guard width > 40 else { return }
        let bold = NSFont.systemFont(ofSize: 13, weight: .bold)
        let boldDigits = NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .bold)
        let resetFont = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .bold)
        let clockFont = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .medium)

        draw(model.title, font: bold, color: .labelColor, x: left, y: 8)
        draw(model.percentText, font: boldDigits, color: model.percentLeft == nil ? .secondaryLabelColor : .labelColor,
             rightAlignedTo: right, y: 8)

        let bar = NSRect(x: left, y: 29, width: width, height: 7)
        let radius = bar.height / 2
        NSColor.quaternaryLabelColor.setFill()
        NSBezierPath(roundedRect: bar, xRadius: radius, yRadius: radius).fill()
        if let percent = model.percentLeft, let level = model.level, percent > 0 {
            var fill = bar
            fill.size.width = max(bar.height, bar.width * CGFloat(percent / 100))
            level.color.setFill()
            NSBezierPath(roundedRect: fill, xRadius: radius, yRadius: radius).fill()
        }

        draw(model.resetText, font: resetFont, color: .secondaryLabelColor, x: left, y: 40)
        if let clock = model.resetClock {
            draw(clock, font: clockFont, color: .secondaryLabelColor, rightAlignedTo: right, y: 40)
        }
    }

    private func draw(_ text: String, font: NSFont, color: NSColor, x: CGFloat, y: CGFloat) {
        (text as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: [.font: font, .foregroundColor: color])
    }

    private func draw(_ text: String, font: NSFont, color: NSColor, rightAlignedTo right: CGFloat, y: CGFloat) {
        let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color]
        let size = (text as NSString).size(withAttributes: attributes)
        (text as NSString).draw(at: NSPoint(x: right - size.width, y: y), withAttributes: attributes)
    }
}
