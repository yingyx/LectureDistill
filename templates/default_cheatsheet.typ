#set page(
  paper: "a4",
  margin: (left: 4mm, right: 4mm, top: 2mm, bottom: 3mm),
)

#let cheat-section-bg = rgb("#222222")
#let cheat-subsec-bg = rgb("#E5E5E5")
#let cheat-accent = rgb("#005A9E")
#let cheat-text = rgb("#111111")
#let cheat-box-bg = rgb("#F7F7F7")
#let cheat-box-frame = rgb("#666666")

#set text(
  font: (
    "Segoe UI",
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "Arial",
    "New Computer Modern",
  ),
  size: 6.6pt,
  lang: "zh",
  fill: cheat-text,
  top-edge: "bounds",
  bottom-edge: "bounds",
)
#set par(
  leading: 3.0pt,
  spacing: 1.2pt,
  first-line-indent: 0pt,
  justify: false,
)

// Default heading spacing - overridden per-level below.
#show heading: set block(above: 0.8pt, below: 0.4pt)

// Section (## / h1): dark background, white text.
#show heading.where(level: 1): it => {
  set block(above: 3pt, below: 2pt)
  set text(font: ("Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"), fill: white, size: 8pt, weight: "bold")
  box(fill: cheat-section-bg, width: 100%, outset: (y: 1pt, x: 1.5pt), inset: (x: 1.4pt, y: 0.8pt), it.body)
}

// Subsection (### / h2): light gray background, blue accent bar.
#show heading.where(level: 2): it => {
  set block(above: 2.2pt, below: 1.6pt)
  set text(font: ("Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"), fill: black, size: 7.2pt, weight: "bold")
  box(fill: cheat-subsec-bg, width: 100%, outset: (y: 0.8pt, x: 1.5pt), inset: (x: 1.2pt, y: 0.8pt))[
    #grid(
      columns: (0.7mm, 1fr),
      gutter: 0.7mm,
      align: horizon,
      rect(width: 0.7mm, height: 1em, fill: cheat-accent),
      it.body,
    )
  ]
}

// Subsubsection (#### / h3): compact blue text.
#show heading.where(level: 3): it => {
  set block(above: 2pt, below: 2.2pt)
  set text(font: ("Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"), fill: cheat-accent, size: 6.9pt, weight: "bold")
  it
}

#set list(spacing: 2.2pt, indent: 1.1em, body-indent: 0.65em, marker: ([•]))
#set enum(spacing: 2.2pt, indent: 1.35em, body-indent: 0.75em, numbering: "1.")
#show list: set block(above: 1pt, below: 1pt, spacing: 1pt)
#show enum: set block(above: 1pt, below: 1pt, spacing: 1pt)
#show math.equation: set block(above: 3.2pt, below: 3.2pt)
#show table: set text(size: 6.6pt)
#show raw: set text(size: 6.5pt)

#let key(body) = text(fill: cheat-accent, weight: "bold", body)
#let term(body) = text(font: ("Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"), weight: "bold", body)
#let cheatimp = $=>$
#let cheatiff = $<=>$
#let cheatsep = block(above: 1pt, below: 1pt)[#line(length: 100%, stroke: 0.25pt + cheat-box-frame)]
#let cheatfact(title, body) = block(above: 2pt, below: 2pt)[
  #rect(
    width: 100%,
    stroke: 0.3pt + cheat-box-frame,
    radius: 1pt,
    inset: 2.4pt,
    fill: cheat-box-bg,
  )[
    #block(fill: cheat-section-bg, width: 100%, inset: (x: 1.5pt, y: 1.2pt))[
      #text(font: ("Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"), size: 7pt, weight: "bold", fill: white, title)
    ]
    #v(1.2pt)
    #body
  ]
]

#columns(3, gutter: 3.2mm)[
{{content}}
]

#context [
  #metadata((
    "kind": "lecture-distill-end-marker",
    "end_page": here().page(),
    "end_y_pt": here().position().y.pt(),
  ))
]
