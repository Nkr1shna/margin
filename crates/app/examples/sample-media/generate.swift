// Generates the fake service's sample source media (macOS, no third-party tools):
//
//   swiftc -O generate.swift -o /tmp/gen-media && /tmp/gen-media <this folder>
//
// square-wave.png             an image source
// oppenheim-ch3.pdf           a 4-page PDF, and its page 3 rendered (oppenheim-ch3-page-3.png)
// lecture-12.m4a              speech from `say`; prints when each sentence starts and ends
// gibbs-demo.mp4              partial sums of a square wave with a spoken soundtrack, and
// gibbs-demo-frame-2200.jpg   its frame at 2.2 s
//
// The fake's citations (src/fake.rs) use the printed sentence times; in the video the
// second sentence runs from about 2.2 s to 4.9 s.

import AppKit
import AVFoundation
import CoreText
import Foundation

let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : ".")
let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("margin-sample-media")
try? FileManager.default.removeItem(at: tmp)
try! FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)

// MARK: drawing

/// Partial sum of the square wave's Fourier series with `terms` odd harmonics.
func squareSum(_ t: Double, terms: Int) -> Double {
    var y = 0.0
    for k in 0..<terms {
        let n = Double(2 * k + 1)
        y += sin(n * t) / n
    }
    return 4 / .pi * y
}

func drawPlot(_ ctx: CGContext, rect: CGRect, terms: [Int], colors: [CGColor], background: CGColor?, axis: CGColor) {
    if let bg = background {
        ctx.setFillColor(bg)
        ctx.fill(rect)
    }
    let midY = rect.midY
    let ampl = rect.height * 0.36
    ctx.setStrokeColor(axis)
    ctx.setLineWidth(1)
    ctx.move(to: CGPoint(x: rect.minX + 8, y: midY))
    ctx.addLine(to: CGPoint(x: rect.maxX - 8, y: midY))
    ctx.strokePath()
    // The ideal square wave, dashed.
    ctx.setLineDash(phase: 0, lengths: [5, 4])
    let periods = 2.0
    let width = rect.width - 16
    ctx.move(to: CGPoint(x: rect.minX + 8, y: midY + ampl))
    for i in 0...Int(periods * 2) {
        let x = rect.minX + 8 + width * CGFloat(Double(i) / (periods * 2))
        let up = i % 2 == 0
        ctx.addLine(to: CGPoint(x: x, y: midY + (up ? ampl : -ampl)))
        if i < Int(periods * 2) {
            let x2 = rect.minX + 8 + width * CGFloat(Double(i + 1) / (periods * 2))
            ctx.addLine(to: CGPoint(x: x2, y: midY + (up ? ampl : -ampl)))
        }
    }
    ctx.strokePath()
    ctx.setLineDash(phase: 0, lengths: [])
    for (i, n) in terms.enumerated() {
        ctx.setStrokeColor(colors[i % colors.count])
        ctx.setLineWidth(2)
        let steps = Int(width)
        for s in 0...steps {
            let t = Double(s) / Double(steps) * periods * 2 * .pi
            let p = CGPoint(x: rect.minX + 8 + CGFloat(s), y: midY + ampl * CGFloat(squareSum(t, terms: n)))
            if s == 0 { ctx.move(to: p) } else { ctx.addLine(to: p) }
        }
        ctx.strokePath()
    }
}

func drawText(_ ctx: CGContext, _ text: String, at: CGPoint, size: CGFloat, color: CGColor, weight: NSFont.Weight = .regular, width: CGFloat = 1000) -> CGFloat {
    let font = NSFont.systemFont(ofSize: size, weight: weight)
    let attr = NSAttributedString(string: text, attributes: [
        .font: font, .foregroundColor: NSColor(cgColor: color) ?? .black,
    ])
    let setter = CTFramesetterCreateWithAttributedString(attr)
    let fit = CTFramesetterSuggestFrameSizeWithConstraints(setter, CFRange(), nil, CGSize(width: width, height: 10000), nil)
    let path = CGPath(rect: CGRect(x: at.x, y: at.y - fit.height, width: width, height: fit.height), transform: nil)
    let frame = CTFramesetterCreateFrame(setter, CFRange(), path, nil)
    ctx.textMatrix = .identity
    ctx.setShouldSmoothFonts(false)
    CTFrameDraw(frame, ctx)
    return fit.height
}

func bitmap(_ w: Int, _ h: Int, _ draw: (CGContext) -> Void) -> CGImage {
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    draw(ctx)
    return ctx.makeImage()!
}

func write(_ image: CGImage, _ name: String, _ type: NSBitmapImageRep.FileType, _ props: [NSBitmapImageRep.PropertyKey: Any] = [:]) {
    let rep = NSBitmapImageRep(cgImage: image)
    try! rep.representation(using: type, properties: props)!.write(to: out.appendingPathComponent(name))
    print("wrote \(name)")
}

let blue = CGColor(red: 0.23, green: 0.51, blue: 0.97, alpha: 1)
let orange = CGColor(red: 0.93, green: 0.49, blue: 0.22, alpha: 1)
let green = CGColor(red: 0.28, green: 0.63, blue: 0.30, alpha: 1)
let ink = CGColor(gray: 0.1, alpha: 1)
let faint = CGColor(gray: 0.6, alpha: 1)

// MARK: image

write(bitmap(800, 450) { ctx in
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: 800, height: 450))
    drawPlot(ctx, rect: CGRect(x: 20, y: 30, width: 760, height: 360), terms: [1, 3, 15],
             colors: [green, orange, blue], background: CGColor(gray: 1, alpha: 1), axis: CGColor(gray: 0.85, alpha: 1))
    _ = drawText(ctx, "Square wave: partial sums with 1, 3 and 15 odd harmonics", at: CGPoint(x: 28, y: 432), size: 18, color: ink, weight: .medium)
}, "square-wave.png", .png)

// MARK: PDF

let pdfURL = out.appendingPathComponent("oppenheim-ch3.pdf")
var media = CGRect(x: 0, y: 0, width: 612, height: 792)
let pdf = CGContext(pdfURL as CFURL, mediaBox: &media, nil)!
let pages: [(String, String)] = [
    ("3.1  A historical perspective", "The idea of representing a signal as a sum of harmonically related sinusoids goes back to the study of vibrating strings and the flow of heat. Fourier claimed that any periodic signal could be written this way; the conditions under which the claim holds were settled later."),
    ("3.2  The response of LTI systems to complex exponentials", "Complex exponentials are eigenfunctions of linear time-invariant systems: the output for input e^{st} is H(s) e^{st}. A signal written as a combination of complex exponentials therefore passes through such a system term by term."),
    ("3.3  Fourier series representation of continuous-time periodic signals", "A periodic signal x(t) with fundamental period T can be represented as a linear combination of harmonically related complex exponentials, each with a frequency that is an integer multiple of the fundamental frequency ω₀ = 2π/T. The term for k = ±1 is the fundamental, or first harmonic; the terms for k = ±N are the N-th harmonics.\n\n    x(t) = Σ a_k e^{jkω₀t},   k = −∞ … ∞\n\nThe coefficients a_k measure how much of each harmonic the signal contains. For the square wave below, only odd harmonics appear."),
    ("3.4  Convergence of the Fourier series", "Near a discontinuity, the partial sums overshoot the signal by about 9% of the jump. Adding terms narrows the overshoot but does not lower it: this is the Gibbs phenomenon. The energy in the difference still goes to zero."),
]
for (i, page) in pages.enumerated() {
    pdf.beginPDFPage(nil)
    var y: CGFloat = 730
    y -= drawText(pdf, "Chapter 3 · Fourier Series Representation of Periodic Signals", at: CGPoint(x: 72, y: y), size: 9, color: faint, width: 468) + 22
    y -= drawText(pdf, page.0, at: CGPoint(x: 72, y: y), size: 17, color: ink, weight: .semibold, width: 468) + 14
    y -= drawText(pdf, page.1, at: CGPoint(x: 72, y: y), size: 11.5, color: ink, width: 468) + 20
    if i == 2 {
        drawPlot(pdf, rect: CGRect(x: 72, y: y - 200, width: 468, height: 190), terms: [1, 3, 7],
                 colors: [green, orange, blue], background: nil, axis: CGColor(gray: 0.8, alpha: 1))
        y -= 214
        _ = drawText(pdf, "Figure 3.6  Partial sums of the square wave's series with 1, 3 and 7 terms.", at: CGPoint(x: 72, y: y), size: 9.5, color: faint, width: 468)
    }
    _ = drawText(pdf, "\(i + 1)", at: CGPoint(x: 300, y: 50), size: 9, color: faint, width: 40)
    pdf.endPDFPage()
}
pdf.closePDF()
print("wrote oppenheim-ch3.pdf")

// Page 3 rendered as the service would (white page, 1.5x).
let doc = CGPDFDocument(pdfURL as CFURL)!
let page3 = doc.page(at: 3)!
write(bitmap(918, 1188) { ctx in
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: 918, height: 1188))
    ctx.scaleBy(x: 1.5, y: 1.5)
    ctx.drawPDFPage(page3)
}, "oppenheim-ch3-page-3.png", .png)

// MARK: speech

func speak(_ text: String, _ name: String) -> URL {
    let url = tmp.appendingPathComponent(name + ".aiff")
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/bin/say")
    p.arguments = ["-r", "185", "-o", url.path, text]
    try! p.run()
    p.waitUntilExit()
    return url
}

func sync<T>(_ body: @escaping () async throws -> T) -> T {
    var result: T?
    let sem = DispatchSemaphore(value: 0)
    Task {
        result = try! await body()
        sem.signal()
    }
    sem.wait()
    return result!
}

func export(_ comp: AVComposition, _ preset: String, _ type: AVFileType, _ name: String) {
    let url = out.appendingPathComponent(name)
    try? FileManager.default.removeItem(at: url)
    let session = AVAssetExportSession(asset: comp, presetName: preset)!
    sync { try await session.export(to: url, as: type) }
    print("wrote \(name)")
}

let lecture = [
    "Lecture twelve. Today we look at what happens near a jump.",
    "Near a discontinuity, the partial sums overshoot by about nine percent of the jump.",
    "Adding more terms squeezes the overshoot closer to the jump, but it never gets smaller.",
    "That stubborn ripple is the Gibbs phenomenon.",
]
let audio = AVMutableComposition()
let audioTrack = audio.addMutableTrack(withMediaType: .audio, preferredTrackID: kCMPersistentTrackID_Invalid)!
var cursor = CMTime.zero
let gap = CMTime(value: 400, timescale: 1000)
for (i, line) in lecture.enumerated() {
    let asset = AVURLAsset(url: speak(line, "lecture-\(i)"))
    let (track, duration) = sync { (try await asset.loadTracks(withMediaType: .audio)[0], try await asset.load(.duration)) }
    try! audioTrack.insertTimeRange(CMTimeRange(start: .zero, duration: duration), of: track, at: cursor)
    let end = cursor + duration
    print(String(format: "lecture %d: %d–%d ms  %@", i, Int(cursor.seconds * 1000), Int(end.seconds * 1000), line))
    cursor = end + gap
}
audio.insertEmptyTimeRange(CMTimeRange(start: cursor - gap, duration: gap))
export(audio, AVAssetExportPresetAppleM4A, .m4a, "lecture-12.m4a")

// MARK: video

let soundtrack = "With one term it's a sine wave. With three terms the shape is already recognisable. With fifteen, only the overshoot at each jump remains."
let speechAsset = AVURLAsset(url: speak(soundtrack, "demo"))
let (speechTrack, speechDuration) = sync { (try await speechAsset.loadTracks(withMediaType: .audio)[0], try await speechAsset.load(.duration)) }
print(String(format: "demo soundtrack: 0–%d ms", Int(speechDuration.seconds * 1000)))
let fps: Int32 = 15
let seconds = Int(ceil(speechDuration.seconds)) + 1
let (vw, vh) = (480, 270)
let silentURL = tmp.appendingPathComponent("frames.mp4")
let writer = try! AVAssetWriter(outputURL: silentURL, fileType: .mp4)
let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
    AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: vw, AVVideoHeightKey: vh,
    AVVideoCompressionPropertiesKey: [AVVideoAverageBitRateKey: 250_000],
])
let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: [
    kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32ARGB, kCVPixelBufferWidthKey as String: vw, kCVPixelBufferHeightKey as String: vh,
])
writer.add(input)
writer.startWriting()
writer.startSession(atSourceTime: .zero)
for f in 0..<(seconds * Int(fps)) {
    let t = Double(f) / Double(fps)
    // 1 term, then 3 from 2 s, then 15 from 5.5 s (as the soundtrack says).
    let terms = t < 2 ? 1 : (t < 5.5 ? 3 : 15)
    var buffer: CVPixelBuffer?
    CVPixelBufferPoolCreatePixelBuffer(nil, adaptor.pixelBufferPool!, &buffer)
    CVPixelBufferLockBaseAddress(buffer!, [])
    let ctx = CGContext(data: CVPixelBufferGetBaseAddress(buffer!), width: vw, height: vh, bitsPerComponent: 8,
                        bytesPerRow: CVPixelBufferGetBytesPerRow(buffer!), space: CGColorSpaceCreateDeviceRGB(),
                        bitmapInfo: CGImageAlphaInfo.noneSkipFirst.rawValue)!
    ctx.setFillColor(CGColor(gray: 0.04, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: vw, height: vh))
    drawPlot(ctx, rect: CGRect(x: 10, y: 10, width: 460, height: 220), terms: [terms], colors: [blue],
             background: CGColor(gray: 0.04, alpha: 1), axis: CGColor(gray: 0.3, alpha: 1))
    _ = drawText(ctx, "\(terms) term\(terms == 1 ? "" : "s")", at: CGPoint(x: 18, y: 258), size: 16, color: CGColor(gray: 1, alpha: 1), weight: .medium)
    CVPixelBufferUnlockBaseAddress(buffer!, [])
    while !input.isReadyForMoreMediaData { usleep(1000) }
    adaptor.append(buffer!, withPresentationTime: CMTime(value: CMTimeValue(f), timescale: fps))
}
input.markAsFinished()
sync { await writer.finishWriting() }

let frames = AVURLAsset(url: silentURL)
let (frameTrack, frameDuration) = sync { (try await frames.loadTracks(withMediaType: .video)[0], try await frames.load(.duration)) }
let movie = AVMutableComposition()
try! movie.addMutableTrack(withMediaType: .video, preferredTrackID: kCMPersistentTrackID_Invalid)!
    .insertTimeRange(CMTimeRange(start: .zero, duration: frameDuration), of: frameTrack, at: .zero)
try! movie.addMutableTrack(withMediaType: .audio, preferredTrackID: kCMPersistentTrackID_Invalid)!
    .insertTimeRange(CMTimeRange(start: .zero, duration: speechDuration), of: speechTrack, at: CMTime(value: 300, timescale: 1000))
export(movie, AVAssetExportPresetMediumQuality, .mp4, "gibbs-demo.mp4")

let generator = AVAssetImageGenerator(asset: AVURLAsset(url: out.appendingPathComponent("gibbs-demo.mp4")))
generator.requestedTimeToleranceBefore = .zero
generator.requestedTimeToleranceAfter = .zero
let frame = sync { try await generator.image(at: CMTime(value: 2200, timescale: 1000)).image }
write(frame, "gibbs-demo-frame-2200.jpg", .jpeg, [.compressionFactor: 0.8])
