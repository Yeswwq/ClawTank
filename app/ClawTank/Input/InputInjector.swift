// Turns client input into macOS events (CGEvent at the HID tap). Requires Accessibility.
//
// State it keeps, because RDP sends deltas of state rather than full snapshots:
// held keys (for modifier flags and auto-repeat), held mouse buttons (drag event types),
// pointer position (relative motion, clicks), click counts (double/triple click), Caps Lock,
// and pending UTF-16 high surrogates. `releaseAll()` lifts everything on disconnect so a
// dropped session never leaves a key or button stuck down on the Mac.

import AppKit
import Carbon.HIToolbox
import CoreGraphics

/// Where events go. Production posts to the system; tests record.
protocol EventPoster: AnyObject {
    func post(_ event: CGEvent)
    func postMediaKey(_ key: ScancodeMap.MediaKey, down: Bool)
}

final class SystemEventPoster: EventPoster {
    func post(_ event: CGEvent) {
        event.post(tap: .cghidEventTap)
    }

    func postMediaKey(_ key: ScancodeMap.MediaKey, down: Bool) {
        let state = down ? 0xA : 0xB
        let event = NSEvent.otherEvent(
            with: .systemDefined, location: .zero, modifierFlags: NSEvent.ModifierFlags(rawValue: UInt(state << 8)),
            timestamp: 0, windowNumber: 0, context: nil, subtype: 8,
            data1: Int((key.rawValue << 16) | Int32(state << 8)), data2: -1)
        event?.cgEvent?.post(tap: .cghidEventTap)
    }
}

/// Client input, as decoded by the core (mirrors `CtInputEvent`).
enum ClientInput: Equatable, Sendable {
    case key(scancode: UInt8, extended: Bool, down: Bool)
    case unicode(UInt16, down: Bool)
    case sync(capsLock: Bool)
    case move(x: Int, y: Int)
    case relativeMove(dx: Int, dy: Int)
    case button(Int, down: Bool)
    case wheel(vertical: Int, horizontal: Int)
    /// RDPEI contact sample. Flags use the MS-RDPEI contactFlags bit layout.
    case touch(contactID: Int, flags: UInt32, x: Int, y: Int)
}

final class InputInjector {
    /// Pixels scrolled per 120 wheel units (one notch).
    static let pixelsPerNotch = 40.0

    var mapper: CoordinateMapper
    var swapControlCommand: Bool

    private let poster: EventPoster
    private let source = CGEventSource(stateID: .hidSystemState)
    private var heldKeys: Set<CGKeyCode> = []
    private var heldButtons: Set<Int> = []
    private var position: CGPoint
    private var capsLock = false
    private var highSurrogate: UInt16?
    private struct Click {
        let button: Int
        let time: TimeInterval
        let point: CGPoint
        let count: Int
    }

    private var lastClick: Click?
    private var scrollRemainder = (vertical: 0.0, horizontal: 0.0)

    // macOS does not expose a public API for injecting arbitrary multitouch contacts into the
    // system trackpad device. We preserve the protocol's contacts here and map the useful,
    // portable semantics onto CGEvent: one finger becomes a click/drag and two fingers become
    // continuous scrolling. This keeps gestures deterministic across macOS and avoids private
    // MultitouchSupport APIs that cannot be shipped or signed reliably.
    private struct TouchContact {
        var point: CGPoint
        var engaged: Bool
    }

    private enum TouchGesture {
        case pendingSingle(id: Int, start: CGPoint)
        case dragging(id: Int)
        case scrolling
    }

    private var touches: [Int: TouchContact] = [:]
    private var touchGesture: TouchGesture?
    private var touchCentroid: CGPoint?

    private static let touchDown: UInt32 = 0x0001
    private static let touchUpdate: UInt32 = 0x0002
    private static let touchUp: UInt32 = 0x0004
    private static let touchInRange: UInt32 = 0x0008
    private static let touchInContact: UInt32 = 0x0010
    private static let touchCanceled: UInt32 = 0x0020
    private static let touchMoveThreshold = 4.0

    init(mapper: CoordinateMapper, swapControlCommand: Bool, poster: EventPoster = SystemEventPoster()) {
        self.mapper = mapper
        self.swapControlCommand = swapControlCommand
        self.poster = poster
        self.position = CGPoint(x: mapper.displayBounds.midX, y: mapper.displayBounds.midY)
    }

    var currentFlags: CGEventFlags {
        var flags = CGEventFlags()
        for key in heldKeys {
            if let flag = ScancodeMap.modifierFlag(for: key) { flags.insert(flag) }
        }
        if capsLock { flags.insert(.maskAlphaShift) }
        return flags
    }

    func handle(_ input: ClientInput) {
        switch input {
        case let .key(scancode, extended, down): key(scancode: scancode, extended: extended, down: down)
        case let .unicode(unit, down): unicode(unit, down: down)
        case let .sync(caps): capsLock = caps
        case let .move(x, y): move(to: mapper.displayPoint(sessionX: Double(x), sessionY: Double(y)))
        case let .relativeMove(dx, dy):
            let scale = mapper.scale
            move(to: mapper.clamp(CGPoint(x: position.x + Double(dx) / scale, y: position.y + Double(dy) / scale)))
        case let .button(button, down): self.button(button, down: down)
        case let .wheel(vertical, horizontal): wheel(vertical: vertical, horizontal: horizontal)
        case let .touch(contactID, flags, x, y): touch(contactID: contactID, flags: flags, x: x, y: y)
        }
    }

    /// Releases every held key and button (call on disconnect).
    func releaseAll() {
        releaseTouches()
        for button in heldButtons.sorted() { self.button(button, down: false) }
        for key in heldKeys.sorted() {
            heldKeys.remove(key)
            postKey(key, down: false, isRepeat: false)
        }
        highSurrogate = nil
    }

    // MARK: Keyboard

    private func key(scancode: UInt8, extended: Bool, down: Bool) {
        switch ScancodeMap.target(scancode: scancode, extended: extended, swapControlCommand: swapControlCommand) {
        case .media(let mediaKey):
            poster.postMediaKey(mediaKey, down: down)
        case .key(let code):
            if Int(code) == kVK_CapsLock {
                if down { capsLock.toggle() }
                postKey(code, down: down, isRepeat: false)
                return
            }
            let isRepeat = down && heldKeys.contains(code)
            if down { heldKeys.insert(code) } else { heldKeys.remove(code) }
            postKey(code, down: down, isRepeat: isRepeat)
        case nil:
            break
        }
    }

    private func postKey(_ code: CGKeyCode, down: Bool, isRepeat: Bool) {
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down) else { return }
        if ScancodeMap.modifierFlag(for: code) != nil || Int(code) == kVK_CapsLock {
            event.type = .flagsChanged
        }
        event.flags = currentFlags
        if isRepeat { event.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        poster.post(event)
    }

    private func unicode(_ unit: UInt16, down: Bool) {
        var units = [unit]
        if UTF16.isLeadSurrogate(unit) {
            if down { highSurrogate = unit }
            return
        }
        if UTF16.isTrailSurrogate(unit) {
            guard let high = highSurrogate else { return }
            units = [high, unit]
            if !down { highSurrogate = nil }
        }
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down) else { return }
        event.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        // Set flags explicitly: otherwise the HID-state source can leak a modifier that was
        // just released (seen live: "é" typed with Shift after Shift+1).
        event.flags = currentFlags
        poster.post(event)
    }

    // MARK: Mouse

    private func move(to point: CGPoint) {
        position = point
        var type = CGEventType.mouseMoved
        var button = CGMouseButton.left
        if heldButtons.contains(0) {
            type = .leftMouseDragged
        } else if heldButtons.contains(1) {
            type = .rightMouseDragged
            button = .right
        } else if let other = heldButtons.min() {
            type = .otherMouseDragged
            button = CGMouseButton(rawValue: UInt32(other)) ?? .center
        }
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point,
                                  mouseButton: button) else { return }
        event.flags = currentFlags
        poster.post(event)
    }

    /// RDP button numbers: 0 left, 1 right, 2 middle, 3 X1/back, 4 X2/forward. These match
    /// CGMouseButton numbering (left 0, right 1, center 2, then 3, 4).
    private func button(_ button: Int, down: Bool) {
        if down { heldButtons.insert(button) } else { heldButtons.remove(button) }
        let type: CGEventType = switch (button, down) {
        case (0, true): .leftMouseDown
        case (0, false): .leftMouseUp
        case (1, true): .rightMouseDown
        case (1, false): .rightMouseUp
        case (_, true): .otherMouseDown
        case (_, false): .otherMouseUp
        }
        let cgButton = CGMouseButton(rawValue: UInt32(button)) ?? .left
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position,
                                  mouseButton: cgButton) else { return }
        event.setIntegerValueField(.mouseEventButtonNumber, value: Int64(button))
        event.setIntegerValueField(.mouseEventClickState, value: Int64(clickCount(button: button, down: down)))
        event.flags = currentFlags
        poster.post(event)
    }

    private func clickCount(button: Int, down: Bool) -> Int {
        let now = ProcessInfo.processInfo.systemUptime
        guard down else { return lastClick?.button == button ? lastClick?.count ?? 1 : 1 }
        var count = 1
        if let last = lastClick, last.button == button, now - last.time <= NSEvent.doubleClickInterval,
           abs(last.point.x - position.x) <= 4, abs(last.point.y - position.y) <= 4 {
            count = last.count + 1
        }
        lastClick = Click(button: button, time: now, point: position, count: count)
        return count
    }

    /// Wheel units (120 per notch) → continuous pixel scrolling. RDP +vertical is "wheel away
    /// from user" (scroll up) like CG's +wheel1; RDP +horizontal is "tilt right", CG's +wheel2 is
    /// left, hence the sign flip.
    private func wheel(vertical: Int, horizontal: Int) {
        scrollRemainder.vertical += Double(vertical) / 120 * Self.pixelsPerNotch
        scrollRemainder.horizontal += Double(-horizontal) / 120 * Self.pixelsPerNotch
        let dy = Int32(scrollRemainder.vertical.rounded(.towardZero))
        let dx = Int32(scrollRemainder.horizontal.rounded(.towardZero))
        scrollRemainder.vertical -= Double(dy)
        scrollRemainder.horizontal -= Double(dx)
        guard dx != 0 || dy != 0,
              let event = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                                  wheel1: dy, wheel2: dx, wheel3: 0) else { return }
        event.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        event.location = position
        event.flags = currentFlags
        poster.post(event)
    }
}

// MARK: RDPEI touch

private extension InputInjector {

    private func touch(contactID: Int, flags: UInt32, x: Int, y: Int) {
        let point = mapper.displayPoint(sessionX: Double(x), sessionY: Double(y))
        let isCanceled = flags & Self.touchCanceled != 0
        let isDown = flags & Self.touchDown != 0
        let isUp = flags & Self.touchUp != 0
        let inContact = flags & Self.touchInContact != 0
        let inRange = flags & Self.touchInRange != 0

        if isDown {
            beginTouch(contactID: contactID, point: point, inContact: inContact)
            return
        }
        guard touches[contactID] != nil || (flags & Self.touchUpdate != 0 && inRange) else { return }
        if isUp || isCanceled || !inRange {
            finishTouch(contactID: contactID, point: point, canceled: isCanceled)
            return
        }
        updateTouch(contactID: contactID, point: point, inContact: inContact)
    }

    private func beginTouch(contactID: Int, point: CGPoint, inContact: Bool) {
        if touches.isEmpty {
            // Delay the click until UP so a second contact can promote the gesture to scrolling.
            touchGesture = .pendingSingle(id: contactID, start: point)
            touchCentroid = point
        } else {
            // A second contact promotes the interaction to scrolling.
            if case .some(.dragging) = touchGesture { button(0, down: false) }
            touchGesture = .scrolling
        }
        touches[contactID] = TouchContact(point: point, engaged: inContact)
        touchCentroid = centroid(of: touches)
    }

    private func finishTouch(contactID: Int, point: CGPoint, canceled: Bool) {
        switch touchGesture {
        case let .some(.pendingSingle(id, _)) where id == contactID:
            if !canceled {
                move(to: point)
                button(0, down: true)
                button(0, down: false)
            }
            touchGesture = nil
        case let .some(.dragging(id)) where id == contactID:
            button(0, down: false)
            touchGesture = nil
        default:
            break
        }
        touches.removeValue(forKey: contactID)
        if touches.isEmpty {
            touchGesture = nil
            touchCentroid = nil
        } else {
            touchCentroid = centroid(of: touches)
        }
    }

    private func updateTouch(contactID: Int, point: CGPoint, inContact: Bool) {
        let oldCentroid = centroid(of: touches)
        touches[contactID] = TouchContact(point: point, engaged: inContact)
        switch touchGesture {
        case let .some(.pendingSingle(id, start)) where id == contactID:
            guard distance(from: start, to: point) > Self.touchMoveThreshold else { return }
            move(to: start)
            button(0, down: true)
            touchGesture = .dragging(id: contactID)
            move(to: point)
        case let .some(.dragging(id)) where id == contactID:
            move(to: point)
        case .some(.scrolling) where touches.count >= 2:
            guard let newCentroid = centroid(of: touches),
                  let previous = touchCentroid ?? oldCentroid else { return }
            scrollPixels(vertical: -(newCentroid.y - previous.y), horizontal: -(newCentroid.x - previous.x))
            position = newCentroid
            touchCentroid = newCentroid
        default:
            break
        }
    }

    private func distance(from lhs: CGPoint, to rhs: CGPoint) -> CGFloat {
        hypot(lhs.x - rhs.x, lhs.y - rhs.y)
    }

    private func centroid(of contacts: [Int: TouchContact]) -> CGPoint? {
        guard !contacts.isEmpty else { return nil }
        let sum = contacts.values.reduce(CGPoint.zero) { partial, contact in
            CGPoint(x: partial.x + contact.point.x, y: partial.y + contact.point.y)
        }
        let count = CGFloat(contacts.count)
        return CGPoint(x: sum.x / count, y: sum.y / count)
    }

    private func scrollPixels(vertical: CGFloat, horizontal: CGFloat) {
        let dy = Int32(vertical.rounded(.towardZero))
        let dx = Int32(horizontal.rounded(.towardZero))
        guard dx != 0 || dy != 0,
              let event = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                                  wheel1: dy, wheel2: dx, wheel3: 0) else { return }
        event.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        event.location = position
        event.flags = currentFlags
        poster.post(event)
    }

    private func releaseTouches() {
        guard !touches.isEmpty || touchGesture != nil else { return }
        if case .some(.dragging) = touchGesture {
            button(0, down: false)
        }
        touches.removeAll(keepingCapacity: true)
        touchGesture = nil
        touchCentroid = nil
    }
}
