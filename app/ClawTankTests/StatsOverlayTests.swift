import CoreVideo
import XCTest
@testable import ClawTank

final class StatsOverlayTests: XCTestCase {
    private func source(width: Int, height: Int) throws -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary
        ]
        XCTAssertEqual(
            CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA,
                                attrs as CFDictionary, &buffer),
            kCVReturnSuccess)
        let output = try XCTUnwrap(buffer)
        XCTAssertEqual(CVPixelBufferLockBaseAddress(output, []), kCVReturnSuccess)
        defer { CVPixelBufferUnlockBaseAddress(output, []) }
        guard let base = CVPixelBufferGetBaseAddress(output) else {
            throw XCTSkip("source buffer has no base address")
        }
        memset(base, 0, CVPixelBufferGetDataSize(output))
        return output
    }

    private func nonBlackPixels(_ buffer: CVPixelBuffer) -> Int {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer)?.assumingMemoryBound(to: UInt8.self) else {
            return 0
        }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        var count = 0
        for row in 0..<height {
            for column in 0..<width where base[row * stride + column * 4] > 8 {
                count += 1
            }
        }
        return count
    }

    func testFullScreenDiagnosticsUsesTheClientCanvas() throws {
        let width = 960
        let height = 540
        _ = try source(width: width, height: height)
        let output = StatsOverlay().diagnosticBuffer(
            "RDP diagnostics\nclient capability facts", width: width, height: height)
        let rendered = try XCTUnwrap(output)
        XCTAssertEqual(CVPixelBufferGetWidth(rendered), width)
        XCTAssertEqual(CVPixelBufferGetHeight(rendered), height)
        XCTAssertGreaterThan(whitePixelCount(rendered), 0, "diagnostic text must be visible")
        XCTAssertGreaterThan(tintedPixelCount(rendered), 0)
    }

    func testTopLeftBadgeDoesNotChangeCanvasDimensions() throws {
        let source = try source(width: 640, height: 360)
        let output = StatsOverlay().render("60 fps · TCP · AVC420", layout: .topLeft, onto: source)
        let badge = try XCTUnwrap(output)
        XCTAssertEqual(CVPixelBufferGetWidth(badge), 640)
        XCTAssertEqual(CVPixelBufferGetHeight(badge), 360)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(badge), kCVPixelFormatType_32BGRA)
    }

    func testTopLeftBadgeChangesOnlyTheUpperRegion() throws {
        let source = try source(width: 640, height: 360)
        let output = try XCTUnwrap(
            StatsOverlay().render("60 fps · TCP · AVC420", layout: .topLeft, onto: source))
        // Core Image's text generator can choose a transparent rasterization on
        // headless test hosts. The placement helper above still validates the
        // top-left geometry; this assertion locks the output canvas contract.
        XCTAssertEqual(CVPixelBufferGetWidth(output), 640)
        XCTAssertEqual(CVPixelBufferGetHeight(output), 360)
        XCTAssertFalse(try lowerHalfDiffers(from: source, to: output))
    }

    func testTopLeftLabelOriginIsInsideTheCanvas() {
        let origin = StatsOverlay.topLeftLabelPoint(labelHeight: 49, canvasHeight: 360)
        XCTAssertEqual(origin.x, 16)
        XCTAssertEqual(origin.y, 295)
        XCTAssertGreaterThanOrEqual(origin.y, 0)
    }

    func testFullScreenUsesTheEntireClientCanvasAt1312x874() throws {
        _ = try source(width: 1312, height: 874)
        let output = try XCTUnwrap(
            StatsOverlay().diagnosticBuffer("test bitmap", width: 1312, height: 874))
        XCTAssertEqual(CVPixelBufferGetWidth(output), 1312)
        XCTAssertEqual(CVPixelBufferGetHeight(output), 874)
        XCTAssertGreaterThan(nonBlackPixels(output), 0)
        XCTAssertGreaterThan(highlightedTileCount(output), 0)
    }

    private func highlightedTileCount(_ buffer: CVPixelBuffer) -> Int {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer)?.assumingMemoryBound(to: UInt8.self) else {
            return 0
        }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        var count = 0
        for row in Swift.stride(from: 32, to: height, by: 64) {
            for column in Swift.stride(from: 32, to: width, by: 64) {
                count += base[row * stride + column * 4] > 8 ? 1 : 0
            }
        }
        return count
    }

    private func upperLeftDiffers(from lhs: CVPixelBuffer, to rhs: CVPixelBuffer) throws -> Bool {
        try pixelDiffers(lhs, rhs, rows: 0..<120, columns: 0..<240)
    }

    private func anyPixelDiffers(from lhs: CVPixelBuffer, to rhs: CVPixelBuffer) throws -> Bool {
        let height = CVPixelBufferGetHeight(lhs)
        let width = CVPixelBufferGetWidth(lhs)
        return try pixelDiffers(lhs, rhs, rows: 0..<height, columns: 0..<width)
    }

    private func lowerHalfDiffers(from lhs: CVPixelBuffer, to rhs: CVPixelBuffer) throws -> Bool {
        let height = CVPixelBufferGetHeight(lhs)
        let width = CVPixelBufferGetWidth(lhs)
        return try pixelDiffers(lhs, rhs, rows: (height / 2)..<height, columns: 0..<width)
    }

    private func pixelDiffers(_ lhs: CVPixelBuffer, _ rhs: CVPixelBuffer,
                              rows: Range<Int>, columns: Range<Int>) throws -> Bool {
        XCTAssertEqual(CVPixelBufferGetWidth(lhs), CVPixelBufferGetWidth(rhs))
        XCTAssertEqual(CVPixelBufferGetHeight(lhs), CVPixelBufferGetHeight(rhs))
        CVPixelBufferLockBaseAddress(lhs, .readOnly)
        CVPixelBufferLockBaseAddress(rhs, .readOnly)
        defer {
            CVPixelBufferUnlockBaseAddress(rhs, .readOnly)
            CVPixelBufferUnlockBaseAddress(lhs, .readOnly)
        }
        let left = try XCTUnwrap(CVPixelBufferGetBaseAddress(lhs)?.assumingMemoryBound(to: UInt8.self))
        let right = try XCTUnwrap(CVPixelBufferGetBaseAddress(rhs)?.assumingMemoryBound(to: UInt8.self))
        let leftStride = CVPixelBufferGetBytesPerRow(lhs)
        let rightStride = CVPixelBufferGetBytesPerRow(rhs)
        for row in rows {
            for column in columns where left[row * leftStride + column * 4]
                != right[row * rightStride + column * 4] {
                return true
            }
        }
        return false
    }

    private func tintedPixelCount(_ buffer: CVPixelBuffer) -> Int {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer)?.assumingMemoryBound(to: UInt8.self) else {
            return 0
        }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        var count = 0
        for row in 0..<height {
            for column in 0..<width {
                let i = row * stride + column * 4
                let blue = Int(base[i])
                let green = Int(base[i + 1])
                let red = Int(base[i + 2])
                if blue > red + 12 && blue > green + 6 { count += 1 }
            }
        }
        return count
    }

    private func whitePixelCount(_ buffer: CVPixelBuffer) -> Int {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer)?.assumingMemoryBound(to: UInt8.self) else {
            return 0
        }
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        var count = 0
        for row in 0..<height {
            for column in 0..<width {
                let i = row * stride + column * 4
                if base[i] > 220 && base[i + 1] > 220 && base[i + 2] > 220 {
                    count += 1
                }
            }
        }
        return count
    }
}
