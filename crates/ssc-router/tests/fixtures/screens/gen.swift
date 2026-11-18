// Generates the CR-20 screen-eval fixtures (synthetic "screens" with one known answer each).
// Run: swift crates/ssc-router/tests/fixtures/screens/gen.swift crates/ssc-router/tests/fixtures/screens
import AppKit

let out = CommandLine.arguments[1]
let W: CGFloat = 1400, H: CGFloat = 800
let font = NSFont.systemFont(ofSize: 30)
let mono = NSFont.monospacedSystemFont(ofSize: 28, weight: .regular)
func attrs(_ f: NSFont = font) -> [NSAttributedString.Key: Any] { [.font: f, .foregroundColor: NSColor.black] }

func screen(_ name: String, chrome: String, draw: () -> Void) {
  let img = NSImage(size: NSSize(width: W, height: H))
  img.lockFocus()
  NSColor.white.setFill(); NSRect(x: 0, y: 0, width: W, height: H).fill()
  NSColor(white: 0.92, alpha: 1).setFill(); NSRect(x: 0, y: H - 50, width: W, height: 50).fill()
  (chrome as NSString).draw(at: NSPoint(x: 20, y: H - 40), withAttributes: [.font: NSFont.systemFont(ofSize: 20), .foregroundColor: NSColor.darkGray])
  draw()
  img.unlockFocus()
  let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
  let jpg = rep.representation(using: .jpeg, properties: [.compressionFactor: 0.85])!
  try! jpg.write(to: URL(fileURLWithPath: "\(out)/\(name).jpg"))
}
func text(_ s: String, _ x: CGFloat, _ y: CGFloat, _ f: NSFont = font) { (s as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: attrs(f)) }
func box(_ x: CGFloat, _ y: CGFloat, _ t: String) {
  let p = NSBezierPath(rect: NSRect(x: x, y: y, width: 160, height: 90)); p.lineWidth = 3; NSColor.black.setStroke(); p.stroke()
  text(t, x + 68, y + 28)
}

screen("diagram_arrow", chrome: "Quiz — Chrome") {
  text("Question 4: In the diagram, which box does the arrow from A point to?", 40, 680)
  box(80, 330, "A"); box(560, 520, "B"); box(560, 120, "C"); box(1040, 330, "D")
  let a = NSBezierPath(); a.lineWidth = 4; a.move(to: NSPoint(x: 240, y: 360)); a.line(to: NSPoint(x: 560, y: 175)); a.stroke()
  let h = NSBezierPath(); h.move(to: NSPoint(x: 560, y: 175)); h.line(to: NSPoint(x: 532, y: 178)); h.line(to: NSPoint(x: 546, y: 202)); h.close(); NSColor.black.setFill(); h.fill()
}
screen("word_inference", chrome: "Microsoft Word — Notes.docx") {
  text("Meeting prep notes", 60, 660, NSFont.boldSystemFont(ofSize: 34))
  text("- Review the roadmap with the team", 60, 590)
  text("- what is inference in ai", 60, 540)
}
screen("math_speed", chrome: "Assessment — Safari") {
  text("Q7. A train travels 180 km in 2.5 hours.", 60, 620)
  text("What is its average speed in km/h?", 60, 570)
  text("Answer: ____________", 60, 480)
}
screen("mcq_planet", chrome: "Trivia Night — Chrome") {
  text("Which planet is known as the Red Planet?", 60, 640)
  for (i, o) in ["A) Venus", "B) Mars", "C) Jupiter", "D) Saturn"].enumerated() { text("○ " + o, 90, 560 - CGFloat(i) * 60) }
}
screen("chart_quarter", chrome: "Revenue 2026 — Excel") {
  text("Which quarter had the highest revenue?", 60, 680)
  let vals: [CGFloat] = [180, 260, 420, 300]
  for (i, v) in vals.enumerated() {
    let x = 200 + CGFloat(i) * 250
    NSColor.systemBlue.setFill(); NSRect(x: x, y: 120, width: 120, height: v).fill()
    text("Q\(i + 1)", x + 40, 80)
  }
  let axis = NSBezierPath(); axis.move(to: NSPoint(x: 150, y: 120)); axis.line(to: NSPoint(x: 1250, y: 120)); NSColor.black.setStroke(); axis.stroke()
}
screen("code_print", chrome: "interview.py — VS Code") {
  text("# What does this program print?", 60, 660, mono)
  text("nums = [1, 2, 3]", 60, 600, mono)
  text("total = 0", 60, 560, mono)
  text("for n in nums:", 60, 520, mono)
  text("    total += n", 60, 480, mono)
  text("print(total)", 60, 440, mono)
}
screen("no_question", chrome: "Weather — Safari") {
  text("Lagos weekend forecast", 60, 660, NSFont.boldSystemFont(ofSize: 34))
  text("Saturday: sunny, 31°C. Sunday: light showers in the afternoon, 29°C.", 60, 590)
  text("Humidity stays high through Monday.", 60, 540)
}
screen("product_price", chrome: "Customer chat — Intercom") {
  text("Customer: Hi! What is the monthly price of your Enterprise plan?", 60, 620)
  text("Reply…", 60, 300)
}
screen("wps_inference", chrome: "WPS Office — Interview prep.docx") {
  // Ribbon noise, then a page with a small question, like a real word-processor window.
  NSColor(white: 0.96, alpha: 1).setFill(); NSRect(x: 0, y: H - 150, width: W, height: 100).fill()
  let small = NSFont.systemFont(ofSize: 16)
  var x: CGFloat = 20
  for t in ["Home", "Insert", "Page Layout", "References", "Review", "View", "Tools", "Paste", "Format Painter", "Calibri", "11", "B", "I", "U", "Styles", "Find and Replace"] {
    text(t, x, H - 110, small); x += CGFloat(t.count) * 9 + 24
  }
  NSColor(white: 0.85, alpha: 1).setFill(); NSRect(x: 0, y: 0, width: 200, height: H - 150).fill()
  NSColor.white.setFill(); NSRect(x: 260, y: 0, width: 880, height: H - 160).fill()
  text("Interview prep", 300, 560, NSFont.boldSystemFont(ofSize: 22))
  text("What is inference in AI?", 300, 510, NSFont.systemFont(ofSize: 18))
  text("Page 1 of 1    6 words    English (US)", 20, 10, small)
}
print("ok")
