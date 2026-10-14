// Builds the binary files of evals/corpus from the text in evals/corpus/sources and the
// drawings below: three PDFs (a text layer, a scanned page with no text layer, and a
// mixed one), four images (two labelled diagrams, two drawn scenes) and the folder
// tree's copies. Run from the repository root:
//
//     swift evals/tools/make_corpus.swift
//
// Everything it draws is written for the corpus (CC0); no outside material is used.
// Output is deterministic apart from the creation dates CoreGraphics writes into PDFs.

import CoreGraphics
import CoreText
import Foundation
import ImageIO
import UniformTypeIdentifiers

let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
    .appendingPathComponent("evals/corpus")

func path(_ rel: String) -> URL { root.appendingPathComponent(rel) }

func pages(of source: String) -> [String] {
    let text = try! String(contentsOf: path("sources/\(source)"), encoding: .utf8)
    return text.components(separatedBy: "\n\\f\n").map {
        $0.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

// MARK: - Text

func font(_ size: CGFloat, bold: Bool = false) -> CTFont {
    CTFontCreateWithName((bold ? "Helvetica-Bold" : "Helvetica") as CFString, size, nil)
}

func attributed(_ page: String, size: CGFloat) -> NSAttributedString {
    let out = NSMutableAttributedString()
    let lines = page.components(separatedBy: "\n")
    for (i, line) in lines.enumerated() {
        // A short line without a full stop is a heading.
        let heading = !line.isEmpty && line.count < 60 && !line.hasSuffix(".")
            && !line.contains("=") && !line.hasPrefix("Example")
        let attrs: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font(
                heading ? size * 1.25 : size, bold: heading),
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): CGColor(
                gray: 0.08, alpha: 1),
        ]
        out.append(NSAttributedString(string: line + (i + 1 < lines.count ? "\n\n" : ""),
                                      attributes: attrs))
    }
    return out
}

/// Lays `page` out in `rect` of `context`.
func draw(_ page: String, in rect: CGRect, of context: CGContext, size: CGFloat) {
    let framesetter = CTFramesetterCreateWithAttributedString(attributed(page, size: size))
    let frame = CTFramesetterCreateFrame(
        framesetter, CFRange(location: 0, length: 0), CGPath(rect: rect, transform: nil), nil)
    CTFrameDraw(frame, context)
}

// MARK: - Bitmaps

func bitmap(_ width: Int, _ height: Int, gray: Bool = false) -> CGContext {
    let space = gray ? CGColorSpaceCreateDeviceGray() : CGColorSpaceCreateDeviceRGB()
    let info = gray ? CGImageAlphaInfo.none.rawValue : CGImageAlphaInfo.noneSkipLast.rawValue
    let ctx = CGContext(
        data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
        space: space, bitmapInfo: info)!
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
    return ctx
}

func writePNG(_ image: CGImage, _ rel: String) {
    let url = path(rel)
    try! FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, image, nil)
    precondition(CGImageDestinationFinalize(dest), "couldn't write \(rel)")
}

func label(_ text: String, at point: CGPoint, in ctx: CGContext, size: CGFloat = 22,
           bold: Bool = false, color: CGColor = CGColor(gray: 0.05, alpha: 1))
{
    let attrs: [NSAttributedString.Key: Any] = [
        NSAttributedString.Key(kCTFontAttributeName as String): font(size, bold: bold),
        NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
    ]
    let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attrs))
    ctx.textPosition = point
    CTLineDraw(line, ctx)
}

func arrow(from a: CGPoint, to b: CGPoint, in ctx: CGContext, width: CGFloat = 3) {
    ctx.setLineWidth(width)
    ctx.move(to: a)
    ctx.addLine(to: b)
    ctx.strokePath()
    let angle = atan2(b.y - a.y, b.x - a.x)
    let len: CGFloat = 16
    ctx.move(to: b)
    ctx.addLine(to: CGPoint(x: b.x - len * cos(angle - 0.4), y: b.y - len * sin(angle - 0.4)))
    ctx.addLine(to: CGPoint(x: b.x - len * cos(angle + 0.4), y: b.y - len * sin(angle + 0.4)))
    ctx.closePath()
    ctx.fillPath()
}

// MARK: - Drawings

/// Pressure-volume diagram of the Carnot cycle with its four steps labelled.
func carnotDiagram() -> CGImage {
    let ctx = bitmap(1200, 900)
    ctx.setStrokeColor(CGColor(gray: 0.1, alpha: 1))
    ctx.setFillColor(CGColor(gray: 0.1, alpha: 1))
    arrow(from: CGPoint(x: 120, y: 100), to: CGPoint(x: 1120, y: 100), in: ctx)
    arrow(from: CGPoint(x: 120, y: 100), to: CGPoint(x: 120, y: 840), in: ctx)
    label("Volume V", at: CGPoint(x: 960, y: 50), in: ctx, size: 26)
    label("Pressure P", at: CGPoint(x: 140, y: 800), in: ctx, size: 26)
    // The cycle in (V, P) units: hot isotherm P = 600 / V from V1 to V2, adiabat
    // P V^1.4 = const down to the cold isotherm P = 300 / V, back along it, and the
    // other adiabat up to the start. Then mapped onto the axes.
    let g: CGFloat = 1.4, kh: CGFloat = 600, kc: CGFloat = 300
    let v1: CGFloat = 1.0, v2: CGFloat = 2.0
    let v3 = v2 * pow(kh / kc, 1 / (g - 1))
    let v4 = v1 * pow(kh / kc, 1 / (g - 1))
    func map(_ v: CGFloat, _ p: CGFloat) -> CGPoint {
        CGPoint(x: 160 + (v - 1) * 85, y: 130 + p * 1.1)
    }
    func curve(_ a: CGFloat, _ b: CGFloat, _ p: (CGFloat) -> CGFloat) -> [CGPoint] {
        (0...100).map { i in
            let v = a + (b - a) * CGFloat(i) / 100
            return map(v, p(v))
        }
    }
    let ca = kh / v2 * pow(v2, g), cb = kh / v1 * pow(v1, g)
    let hot = curve(v1, v2) { kh / $0 }
    let a1 = curve(v2, v3) { ca / pow($0, g) }
    let cold = curve(v3, v4) { kc / $0 }
    let a2 = curve(v4, v1) { cb / pow($0, g) }
    ctx.setLineWidth(5)
    for (line, color) in [
        (hot, CGColor(red: 0.8, green: 0.15, blue: 0.1, alpha: 1)),
        (a1, CGColor(gray: 0.2, alpha: 1)),
        (cold, CGColor(red: 0.1, green: 0.3, blue: 0.8, alpha: 1)),
        (a2, CGColor(gray: 0.2, alpha: 1)),
    ] {
        ctx.setStrokeColor(color)
        ctx.addLines(between: line)
        ctx.strokePath()
    }
    label("Carnot cycle", at: CGPoint(x: 700, y: 820), in: ctx, size: 34, bold: true)
    label("isothermal expansion at T_hot", at: CGPoint(x: 260, y: 560), in: ctx)
    label("adiabatic expansion", at: CGPoint(x: 520, y: 230), in: ctx)
    label("isothermal compression at T_cold", at: CGPoint(x: 560, y: 120), in: ctx)
    label("adiabatic compression (left curve)", at: CGPoint(x: 330, y: 470), in: ctx)
    label("heat in Q_h", at: CGPoint(x: 220, y: 760), in: ctx,
          color: CGColor(red: 0.8, green: 0.15, blue: 0.1, alpha: 1))
    label("heat out Q_c", at: CGPoint(x: 900, y: 190), in: ctx,
          color: CGColor(red: 0.1, green: 0.3, blue: 0.8, alpha: 1))
    label("net work = enclosed area", at: CGPoint(x: 600, y: 420), in: ctx, size: 20)
    return ctx.makeImage()!
}

/// The Calvin cycle as a loop of its three phases.
func calvinDiagram() -> CGImage {
    let ctx = bitmap(1200, 900)
    let center = CGPoint(x: 600, y: 440)
    ctx.setStrokeColor(CGColor(red: 0.1, green: 0.45, blue: 0.2, alpha: 1))
    ctx.setLineWidth(8)
    ctx.addArc(center: center, radius: 260, startAngle: 0, endAngle: 2 * .pi, clockwise: false)
    ctx.strokePath()
    ctx.setFillColor(CGColor(red: 0.1, green: 0.45, blue: 0.2, alpha: 1))
    for angle in [CGFloat.pi * 0.15, .pi * 0.85, .pi * 1.5] {
        let p = CGPoint(x: center.x + 260 * cos(angle), y: center.y + 260 * sin(angle))
        let d = CGPoint(x: -sin(angle) * 30, y: cos(angle) * 30)
        arrow(from: CGPoint(x: p.x - d.x, y: p.y - d.y), to: CGPoint(x: p.x + d.x, y: p.y + d.y),
              in: ctx, width: 8)
    }
    label("The Calvin cycle", at: CGPoint(x: 440, y: 840), in: ctx, size: 36, bold: true)
    label("1. CO2 fixation by RuBisCO", at: CGPoint(x: 800, y: 690), in: ctx, size: 24, bold: true)
    label("CO2 + RuBP -> 2 x 3-phosphoglycerate", at: CGPoint(x: 800, y: 655), in: ctx, size: 20)
    label("2. Reduction", at: CGPoint(x: 70, y: 640), in: ctx, size: 24, bold: true)
    label("ATP and NADPH make G3P", at: CGPoint(x: 70, y: 605), in: ctx, size: 20)
    label("3. RuBP regeneration", at: CGPoint(x: 470, y: 120), in: ctx, size: 24, bold: true)
    label("uses ATP", at: CGPoint(x: 550, y: 85), in: ctx, size: 20)
    label("G3P out to sugars", at: CGPoint(x: 70, y: 260), in: ctx, size: 22)
    label("3 CO2 in : 1 G3P out", at: CGPoint(x: 480, y: 440), in: ctx, size: 26, bold: true)
    label("9 ATP, 6 NADPH", at: CGPoint(x: 515, y: 400), in: ctx, size: 22)
    return ctx.makeImage()!
}

/// The three principal rays for an object at 30 cm from a lens with f = 10 cm.
func rayDiagram() -> CGImage {
    let ctx = bitmap(1400, 700)
    let axisY: CGFloat = 330
    let lensX: CGFloat = 700
    let scale: CGFloat = 18 // pixels per cm
    ctx.setStrokeColor(CGColor(gray: 0.3, alpha: 1))
    ctx.setLineWidth(2)
    ctx.move(to: CGPoint(x: 40, y: axisY))
    ctx.addLine(to: CGPoint(x: 1360, y: axisY))
    ctx.strokePath()
    label("principal axis", at: CGPoint(x: 1180, y: axisY - 32), in: ctx, size: 20)
    // The lens.
    ctx.setStrokeColor(CGColor(red: 0.2, green: 0.4, blue: 0.8, alpha: 1))
    ctx.setLineWidth(6)
    ctx.addEllipse(in: CGRect(x: lensX - 18, y: axisY - 260, width: 36, height: 520))
    ctx.strokePath()
    label("converging lens", at: CGPoint(x: lensX - 80, y: 630), in: ctx, size: 22, bold: true)
    for (name, cm) in [("F", -10.0), ("F", 10.0), ("2F", -20.0), ("2F", 20.0)] as [(String, CGFloat)] {
        let x = lensX + cm * scale
        ctx.setFillColor(CGColor(gray: 0, alpha: 1))
        ctx.fillEllipse(in: CGRect(x: x - 6, y: axisY - 6, width: 12, height: 12))
        label(name, at: CGPoint(x: x - 10, y: axisY - 40), in: ctx, size: 22)
    }
    // Object (upright, 6 cm tall) and image (inverted, 3 cm).
    let objX = lensX - 30 * scale
    let imgX = lensX + 15 * scale
    let objTop = CGPoint(x: objX, y: axisY + 6 * scale)
    let imgTip = CGPoint(x: imgX, y: axisY - 3 * scale)
    ctx.setFillColor(CGColor(red: 0.1, green: 0.5, blue: 0.2, alpha: 1))
    ctx.setStrokeColor(CGColor(red: 0.1, green: 0.5, blue: 0.2, alpha: 1))
    arrow(from: CGPoint(x: objX, y: axisY), to: objTop, in: ctx, width: 6)
    label("object", at: CGPoint(x: objX - 40, y: objTop.y + 20), in: ctx, size: 22)
    ctx.setFillColor(CGColor(red: 0.7, green: 0.2, blue: 0.2, alpha: 1))
    ctx.setStrokeColor(CGColor(red: 0.7, green: 0.2, blue: 0.2, alpha: 1))
    arrow(from: CGPoint(x: imgX, y: axisY), to: imgTip, in: ctx, width: 6)
    label("real inverted image", at: CGPoint(x: imgX - 60, y: imgTip.y - 36), in: ctx, size: 22)
    // Rays.
    ctx.setStrokeColor(CGColor(red: 0.85, green: 0.55, blue: 0.0, alpha: 1))
    ctx.setLineWidth(3)
    ctx.addLines(between: [objTop, CGPoint(x: lensX, y: objTop.y), imgTip])
    ctx.addLines(between: [objTop, imgTip])
    let fFront = CGPoint(x: lensX - 10 * scale, y: axisY)
    let hit = CGPoint(x: lensX, y: objTop.y + (fFront.y - objTop.y) * (lensX - objTop.x) / (fFront.x - objTop.x))
    ctx.addLines(between: [objTop, hit, CGPoint(x: imgX + 200, y: hit.y)])
    ctx.strokePath()
    label("d_o = 30 cm", at: CGPoint(x: objX + 120, y: 110), in: ctx, size: 22)
    label("d_i = 15 cm", at: CGPoint(x: lensX + 60, y: 110), in: ctx, size: 22)
    label("f = 10 cm, magnification m = -0.5", at: CGPoint(x: 420, y: 40), in: ctx, size: 22)
    return ctx.makeImage()!
}

/// A sailboat on a lake at sunset: a drawn scene with no text.
func sailboatScene() -> CGImage {
    let w = 1024, h = 768
    let ctx = bitmap(w, h)
    let sky = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [
            CGColor(red: 0.98, green: 0.55, blue: 0.20, alpha: 1),
            CGColor(red: 0.85, green: 0.35, blue: 0.45, alpha: 1),
            CGColor(red: 0.25, green: 0.20, blue: 0.50, alpha: 1),
        ] as CFArray, locations: [0, 0.45, 1])!
    ctx.drawLinearGradient(sky, start: CGPoint(x: 0, y: 300), end: CGPoint(x: 0, y: h), options: [])
    ctx.setFillColor(CGColor(red: 1.0, green: 0.85, blue: 0.4, alpha: 1))
    ctx.fillEllipse(in: CGRect(x: 600, y: 270, width: 140, height: 140))
    let water = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [
            CGColor(red: 0.10, green: 0.15, blue: 0.35, alpha: 1),
            CGColor(red: 0.55, green: 0.35, blue: 0.40, alpha: 1),
        ] as CFArray, locations: [0, 1])!
    ctx.saveGState()
    ctx.clip(to: CGRect(x: 0, y: 0, width: w, height: 330))
    ctx.drawLinearGradient(water, start: CGPoint(x: 0, y: 0), end: CGPoint(x: 0, y: 330), options: [])
    ctx.restoreGState()
    ctx.setStrokeColor(CGColor(red: 1, green: 0.8, blue: 0.5, alpha: 0.6))
    ctx.setLineWidth(3)
    for i in 0..<14 {
        let y = CGFloat(40 + i * 20)
        let half = CGFloat(20 + i * 6)
        ctx.move(to: CGPoint(x: 670 - half, y: y))
        ctx.addLine(to: CGPoint(x: 670 + half, y: y))
    }
    ctx.strokePath()
    // Hills.
    ctx.setFillColor(CGColor(red: 0.12, green: 0.10, blue: 0.22, alpha: 1))
    ctx.move(to: CGPoint(x: 0, y: 330))
    ctx.addCurve(to: CGPoint(x: 420, y: 330), control1: CGPoint(x: 120, y: 470),
                 control2: CGPoint(x: 280, y: 420))
    ctx.closePath()
    ctx.fillPath()
    // The boat: hull, mast and two sails.
    ctx.setFillColor(CGColor(red: 0.08, green: 0.06, blue: 0.10, alpha: 1))
    ctx.move(to: CGPoint(x: 250, y: 200))
    ctx.addLine(to: CGPoint(x: 470, y: 200))
    ctx.addLine(to: CGPoint(x: 430, y: 160))
    ctx.addLine(to: CGPoint(x: 280, y: 160))
    ctx.closePath()
    ctx.fillPath()
    ctx.fill(CGRect(x: 355, y: 200, width: 8, height: 300))
    ctx.setFillColor(CGColor(red: 0.95, green: 0.92, blue: 0.85, alpha: 1))
    ctx.move(to: CGPoint(x: 368, y: 495))
    ctx.addLine(to: CGPoint(x: 368, y: 215))
    ctx.addLine(to: CGPoint(x: 470, y: 215))
    ctx.closePath()
    ctx.fillPath()
    ctx.move(to: CGPoint(x: 350, y: 460))
    ctx.addLine(to: CGPoint(x: 350, y: 215))
    ctx.addLine(to: CGPoint(x: 270, y: 215))
    ctx.closePath()
    ctx.fillPath()
    return ctx.makeImage()!
}

/// A red bicycle leaning against a brick wall: a drawn scene with no text.
func bicycleScene() -> CGImage {
    let w = 1024, h = 768
    let ctx = bitmap(w, h)
    // Bricks.
    var rng: UInt64 = 0x9E3779B97F4A7C15
    func next() -> CGFloat {
        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17
        return CGFloat(rng % 1000) / 1000
    }
    ctx.setFillColor(CGColor(red: 0.75, green: 0.72, blue: 0.68, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 120, width: w, height: h - 120))
    let bw: CGFloat = 96, bh: CGFloat = 40
    var row = 0
    var y: CGFloat = 120
    while y < CGFloat(h) {
        var x: CGFloat = row % 2 == 0 ? 0 : -bw / 2
        while x < CGFloat(w) {
            let shade = 0.55 + next() * 0.15
            ctx.setFillColor(CGColor(red: shade, green: shade * 0.45, blue: shade * 0.35, alpha: 1))
            ctx.fill(CGRect(x: x + 3, y: y + 3, width: bw - 6, height: bh - 6))
            x += bw
        }
        y += bh
        row += 1
    }
    // Pavement.
    ctx.setFillColor(CGColor(red: 0.45, green: 0.45, blue: 0.47, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: 120))
    // Bicycle.
    let back = CGPoint(x: 330, y: 230), front = CGPoint(x: 700, y: 230)
    ctx.setStrokeColor(CGColor(gray: 0.08, alpha: 1))
    ctx.setLineWidth(14)
    for c in [back, front] {
        ctx.strokeEllipse(in: CGRect(x: c.x - 130, y: c.y - 130, width: 260, height: 260))
    }
    ctx.setLineWidth(2)
    for c in [back, front] {
        for k in 0..<18 {
            let a = CGFloat(k) * .pi / 9
            ctx.move(to: c)
            ctx.addLine(to: CGPoint(x: c.x + 124 * cos(a), y: c.y + 124 * sin(a)))
        }
    }
    ctx.strokePath()
    let crank = CGPoint(x: 490, y: 240), seat = CGPoint(x: 450, y: 450), head = CGPoint(x: 650, y: 460)
    ctx.setStrokeColor(CGColor(red: 0.85, green: 0.08, blue: 0.10, alpha: 1))
    ctx.setLineWidth(16)
    ctx.setLineCap(.round)
    ctx.addLines(between: [back, crank, seat, back])
    ctx.addLines(between: [crank, head, seat])
    ctx.addLines(between: [head, front])
    ctx.strokePath()
    ctx.setStrokeColor(CGColor(gray: 0.1, alpha: 1))
    ctx.setLineWidth(12)
    ctx.addLines(between: [seat, CGPoint(x: 440, y: 500)])
    ctx.addLines(between: [CGPoint(x: 400, y: 505), CGPoint(x: 490, y: 505)])
    ctx.addLines(between: [head, CGPoint(x: 640, y: 520), CGPoint(x: 720, y: 530)])
    ctx.strokePath()
    return ctx.makeImage()!
}

/// `text` rendered like a page from a flatbed scanner: grey, slightly rotated, with
/// speckles, and no text layer.
func scannedPage(_ text: String) -> CGImage {
    let w = 1275, h = 1650 // US Letter at 150 dpi
    let ctx = bitmap(w, h, gray: true)
    ctx.setFillColor(CGColor(gray: 0.94, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    var rng: UInt64 = 0xD1B54A32D192ED03
    func next() -> CGFloat {
        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17
        return CGFloat(rng % 10_000) / 10_000
    }
    ctx.setFillColor(CGColor(gray: 0.6, alpha: 1))
    for _ in 0..<900 {
        ctx.fill(CGRect(x: next() * CGFloat(w), y: next() * CGFloat(h), width: 2, height: 2))
    }
    ctx.saveGState()
    ctx.translateBy(x: CGFloat(w) / 2, y: CGFloat(h) / 2)
    ctx.rotate(by: 0.006)
    ctx.translateBy(x: -CGFloat(w) / 2, y: -CGFloat(h) / 2)
    draw(text, in: CGRect(x: 130, y: 150, width: CGFloat(w) - 260, height: CGFloat(h) - 300),
         of: ctx, size: 25)
    ctx.restoreGState()
    return ctx.makeImage()!
}

// MARK: - PDFs

let letter = CGRect(x: 0, y: 0, width: 612, height: 792)

func pdf(_ rel: String, _ body: (CGContext) -> Void) {
    let url = path(rel)
    try! FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    var box = letter
    let info: [CFString: Any] = [
        kCGPDFContextTitle: url.deletingPathExtension().lastPathComponent,
        kCGPDFContextCreator: "Margin evaluation corpus",
    ]
    let ctx = CGContext(url as CFURL, mediaBox: &box, info as CFDictionary)!
    body(ctx)
    ctx.closePDF()
}

func textPage(_ ctx: CGContext, _ text: String) {
    ctx.beginPDFPage(nil)
    draw(text, in: letter.insetBy(dx: 64, dy: 64), of: ctx, size: 11)
    ctx.endPDFPage()
}

pdf("pdf/rc-circuits.pdf") { ctx in
    for page in pages(of: "rc-circuits.txt") { textPage(ctx, page) }
}

pdf("pdf/glacier-scan.pdf") { ctx in
    let scan = scannedPage(pages(of: "glacier-scan.txt")[0])
    ctx.beginPDFPage(nil)
    ctx.draw(scan, in: letter)
    ctx.endPDFPage()
}

let rays = rayDiagram()
pdf("pdf/lens-optics.pdf") { ctx in
    let text = pages(of: "lens-optics.txt")
    textPage(ctx, text[0])
    ctx.beginPDFPage(nil)
    draw(text[1], in: CGRect(x: 64, y: 620, width: 484, height: 108), of: ctx, size: 11)
    ctx.draw(rays, in: CGRect(x: 56, y: 300, width: 500, height: 250))
    ctx.endPDFPage()
}

writePNG(carnotDiagram(), "images/carnot-pv-diagram.png")
writePNG(calvinDiagram(), "images/calvin-cycle.png")
writePNG(sailboatScene(), "images/scene-sailboat-sunset.png")
writePNG(bicycleScene(), "images/scene-red-bicycle.png")
writePNG(rays, "folder/figures/lens-ray-diagram.png")
try? FileManager.default.removeItem(at: path("folder/papers/rc-circuits.pdf"))
try! FileManager.default.createDirectory(at: path("folder/papers"), withIntermediateDirectories: true)
try! FileManager.default.copyItem(at: path("pdf/rc-circuits.pdf"), to: path("folder/papers/rc-circuits.pdf"))
print("corpus written to \(root.path)")
