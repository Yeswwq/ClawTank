// Optional on-video stats readout (fps, bitrate, encode time), drawn with Core Image into a
// pooled copy of the frame on the GPU. Only used while the overlay is switched on, so the
// normal path stays zero-copy.

import CoreImage
import CoreVideo
import Foundation

final class StatsOverlay: @unchecked Sendable {
    enum Layout: Equatable {
        case topLeft
        case fullScreen
    }

    private struct BufferKey: Equatable {
        let width: Int
        let height: Int
        let format: OSType
    }

    private let context = CIContext(options: [.cacheIntermediates: false])
    private var pool: CVPixelBufferPool?
    private var poolKey: BufferKey?
    private var cachedText = ""
    private var cachedLayout: Layout?
    private var cachedFontSize = 0
    private var cachedImage: CIImage?

    /// Returns a new buffer with `text` drawn in `layout`, or nil if a buffer couldn't be made.
    /// `fullScreen` is used for the initial test bitmap and paints an opaque diagnostic canvas;
    /// `topLeft` is the compact live-video badge. Rendering stays on VideoSession's serial
    /// encode queue because this object owns a mutable pool and text cache.
    func render(_ text: String, layout: Layout = .topLeft, onto source: CVPixelBuffer) -> CVPixelBuffer? {
        let width = CVPixelBufferGetWidth(source)
        let height = CVPixelBufferGetHeight(source)
        let format = CVPixelBufferGetPixelFormatType(source)
        guard let output = makeBuffer(width: width, height: height, format: format)
        else { return nil }

        let baseFontSize = layout == .fullScreen
            ? max(18, min(36, width / 36))
            : max(12, min(28, width / 54))
        let fontSize = fittedFontSize(text, layout: layout, base: baseFontSize,
                                      width: width, height: height)
        if text != cachedText || layout != cachedLayout || fontSize != cachedFontSize || cachedImage == nil {
            let filter = CIFilter(name: "CITextImageGenerator")
            filter?.setValue(text, forKey: "inputText")
            filter?.setValue("Menlo-Bold", forKey: "inputFontName")
            filter?.setValue(fontSize, forKey: "inputFontSize")
            filter?.setValue(8, forKey: "inputPadding")
            cachedImage = filter?.outputImage?.whiteText()
            cachedText = text
            cachedLayout = layout
            cachedFontSize = fontSize
        }
        guard let label = cachedImage else { return nil }
        let size = CGSize(width: width, height: height)
        let scene = layout == .topLeft
            ? topLeftScene(label, source: source, size: size, height: height)
            : fullScreenScene(label, size: size, width: width)
        context.render(scene, to: output)
        return output
    }

    private func fittedFontSize(_ text: String, layout: Layout, base: Int,
                                width: Int, height: Int) -> Int {
        var candidate = base
        while candidate > 10 {
            let extent = Self.textExtent(text, fontSize: candidate)
            let maxWidth = layout == .fullScreen ? CGFloat(width) - 80 : CGFloat(width) - 32
            let maxHeight = layout == .fullScreen ? CGFloat(height) - 80 : CGFloat(height) - 32
            if extent.width <= maxWidth && extent.height <= maxHeight { return candidate }
            candidate -= 1
        }
        return candidate
    }

    private func topLeftScene(_ label: CIImage, source: CVPixelBuffer,
                              size: CGSize, height: Int) -> CIImage {
        let backdrop = CIImage(color: CIColor(red: 0, green: 0, blue: 0, alpha: 0.62))
            .cropped(to: label.extent)
        let origin = Self.topLeftLabelPoint(
            labelHeight: label.extent.height, canvasHeight: CGFloat(height))
        let badge = label.composited(over: backdrop)
            .transformed(by: CGAffineTransform(translationX: origin.x, y: origin.y))
        return badge.composited(over: CIImage(cvPixelBuffer: source).clampedToExtent())
            .cropped(to: CGRect(origin: .zero, size: size))
    }

    private func fullScreenScene(_ label: CIImage, size: CGSize, width: Int) -> CIImage {
        let canvas = CIImage(color: CIColor(red: 0.035, green: 0.055, blue: 0.09, alpha: 1))
            .cropped(to: CGRect(origin: .zero, size: size))
        let labelOrigin = CGPoint(
            x: max(16, min(CGFloat(width) * 0.06, 96)),
            y: max(16, size.height * 0.5 - label.extent.height * 0.5))
        let panelPadding = CGFloat(18)
        let panel = CGRect(
            x: max(12, labelOrigin.x - panelPadding),
            y: max(12, labelOrigin.y - panelPadding),
            width: min(label.extent.width + panelPadding * 2, size.width - 24),
            height: min(label.extent.height + panelPadding * 2, size.height - 24))
        let backdrop = CIImage(color: CIColor(red: 0, green: 0, blue: 0, alpha: 0.56))
            .cropped(to: panel)
        let placed = label.transformed(by: CGAffineTransform(
            translationX: labelOrigin.x, y: labelOrigin.y))
        return placed.composited(over: backdrop).composited(over: canvas)
    }

    nonisolated static func sourceTextWidthForTest(_ text: String, fontSize: CGFloat) -> CGFloat {
        textExtent(text, fontSize: Int(fontSize)).width
    }

    nonisolated static func textExtent(_ text: String, fontSize: Int) -> CGRect {
        let filter = CIFilter(name: "CITextImageGenerator")
        filter?.setValue(text, forKey: "inputText")
        filter?.setValue("Menlo-Bold", forKey: "inputFontName")
        filter?.setValue(fontSize, forKey: "inputFontSize")
        filter?.setValue(8, forKey: "inputPadding")
        return filter?.outputImage?.extent ?? .zero
    }

    private func makeBuffer(width: Int, height: Int, format: OSType) -> CVPixelBuffer? {
        if pool == nil || poolKey?.width != width || poolKey?.height != height || poolKey?.format != format {
            let attributes: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: format,
                kCVPixelBufferWidthKey: width,
                kCVPixelBufferHeightKey: height,
                kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary
            ]
            var created: CVPixelBufferPool?
            CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &created)
            pool = created
            poolKey = BufferKey(width: width, height: height, format: format)
        }
        guard let pool else { return nil }
        var buffer: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer)
        return buffer
    }

    nonisolated static func layoutForTestPattern(width: Int, height: Int) -> CGRect {
        CGRect(x: 0, y: 0, width: width, height: height)
    }

    nonisolated static func topLeftLabelPoint(labelHeight: CGFloat, canvasHeight: CGFloat) -> CGPoint {
        CGPoint(x: 16, y: canvasHeight - labelHeight - 16)
    }

    /// Creates a standalone BGRA canvas for the Rust-side initial test bitmap.
    func diagnosticBuffer(_ text: String, width: Int, height: Int) -> CVPixelBuffer? {
        var source: CVPixelBuffer?
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height
        ]
        guard CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA,
                                  attributes as CFDictionary, &source) == kCVReturnSuccess,
              let source else { return nil }
        CVPixelBufferLockBaseAddress(source, [])
        if let base = CVPixelBufferGetBaseAddress(source) {
            memset(base, 0, CVPixelBufferGetDataSize(source))
        }
        CVPixelBufferUnlockBaseAddress(source, [])
        return render(text, layout: .fullScreen, onto: source)
    }
}

private extension CIImage {
    /// CITextImageGenerator renders opaque black text by default. Force every
    /// non-transparent source pixel to white so the diagnostics stay legible on
    /// the dark canvas and the compact badge.
    func whiteText() -> CIImage {
        applyingFilter("CIColorMatrix", parameters: [
            "inputRVector": CIVector(x: 0, y: 0, z: 0, w: 0),
            "inputGVector": CIVector(x: 0, y: 0, z: 0, w: 0),
            "inputBVector": CIVector(x: 0, y: 0, z: 0, w: 0),
            "inputAVector": CIVector(x: 0, y: 0, z: 0, w: 1),
            "inputBiasVector": CIVector(x: 1, y: 1, z: 1, w: 0)
        ])
    }
}
