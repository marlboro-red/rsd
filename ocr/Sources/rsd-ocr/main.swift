// rsd-ocr — Vision text recognition helper. A separate process (its own
// sandbox boundary) invoked by the daemon's OCR content source with an image
// path; prints recognized text to stdout, one line per observation.
//
//   rsd-ocr <image>                 → OCR, text to stdout
//   rsd-ocr --render <text> <out>   → render text to a PNG (tests/demos)

import Foundation
import Vision
import AppKit
import CoreGraphics
import CoreText

func fail(_ msg: String, _ code: Int32) -> Never {
    FileHandle.standardError.write((msg + "\n").data(using: .utf8)!)
    exit(code)
}

func render(_ text: String, _ out: String) {
    let w = 900, h = 260
    let cs = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8,
                              bytesPerRow: 0, space: cs,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { fail("context", 1) }
    ctx.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 44),
        .foregroundColor: NSColor.black,
    ]
    let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attrs))
    ctx.textPosition = CGPoint(x: 40, y: h / 2)
    CTLineDraw(line, ctx)
    guard let img = ctx.makeImage() else { fail("image", 1) }
    let rep = NSBitmapImageRep(cgImage: img)
    guard let png = rep.representation(using: .png, properties: [:]) else { fail("png", 1) }
    try? png.write(to: URL(fileURLWithPath: out))
}

func ocr(_ path: String) {
    guard let image = NSImage(contentsOfFile: path),
          let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil)
    else { print(""); exit(0) }  // not an image / unreadable → empty, never an error
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    let handler = VNImageRequestHandler(cgImage: cg, options: [:])
    try? handler.perform([request])
    var out = ""
    for obs in (request.results ?? []) {
        if let top = obs.topCandidates(1).first { out += top.string + "\n" }
    }
    FileHandle.standardOutput.write(out.data(using: .utf8)!)
}

let args = CommandLine.arguments
if args.count == 4 && args[1] == "--render" {
    render(args[2], args[3])
} else if args.count == 2 {
    ocr(args[1])
} else {
    fail("usage: rsd-ocr <image> | rsd-ocr --render <text> <out.png>", 2)
}
