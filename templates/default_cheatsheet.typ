#set page(
  paper: "a4",
  margin: (x: 0.5mm, y: 0.5mm),
)
#set text(
  font: ("Microsoft YaHei", "SimSun", "Noto Sans CJK SC", "Arial", "New Computer Modern"),
  size: 5pt,
  lang: "zh",
)
#set par(
  leading: 0pt,
  spacing: 0pt,
  first-line-indent: 0pt,
  justify: false,
)
#show heading: set block(above: 0.8pt, below: 0.4pt)
#show heading.where(level: 1): set text(fill: rgb("#004D80"), size: 7pt, weight: "bold")
#show heading.where(level: 2): set text(fill: rgb("#004D80"), size: 6pt, weight: "bold")
#show heading.where(level: 3): set text(fill: rgb("#0076BA"), size: 5.5pt, weight: "bold")
#show list: set block(spacing: 0pt)
#show enum: set block(spacing: 0pt)

#columns(4, gutter: 2mm)[
{{content}}
]
