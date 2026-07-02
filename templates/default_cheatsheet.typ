#set page(
  paper: "a4",
  margin: (left: 4mm, right: 4mm, top: 2mm, bottom: 3mm),
)
#set text(
  font: ("Microsoft YaHei", "SimSun", "Noto Sans CJK SC", "Arial", "New Computer Modern"),
  size: 6.5pt,
  lang: "zh",
)
#set par(
  leading: 0.5pt,
  spacing: 0.5pt,
  first-line-indent: 0pt,
  justify: false,
)

// Default heading spacing — overridden per-level below
#show heading: set block(above: 0.8pt, below: 0.4pt)

// Section (## / h1): dark background, white text
#show heading.where(level: 1): it => {
  set block(above: 2.2pt, below: 1pt)
  set text(fill: white, size: 7.5pt, weight: "bold")
  box(fill: rgb("#222222"), width: 100%, outset: (y: 1pt, x: 1.5pt), it.body)
}

// Subsection (### / h2): light gray background, blue accent bar
#show heading.where(level: 2): it => {
  set block(above: 1.5pt, below: 0.5pt)
  set text(fill: black, size: 6.8pt, weight: "bold")
  box(fill: rgb("#E5E5E5"), width: 100%, outset: (y: 0.8pt, x: 1.5pt))[
    #rect(width: 0.7mm, height: 1.6mm, fill: rgb("#005A9E"))
    #h(0.6mm)
    #it.body
  ]
}

// Subsubsection (#### / h3): blue text, run-in style
#show heading.where(level: 3): it => {
  set block(above: 1pt, below: 0.2pt)
  set text(fill: rgb("#005A9E"), size: 6.3pt, weight: "bold")
  it
}

#show list: set block(spacing: 0pt)
#show enum: set block(spacing: 0pt)

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
