from PyQt6.QtGui import QColor

# High-contrast variant — pumped up for cheap/TN panels
C = {
    'base':     '#1a1a28',   # terminal bg — dark but not pitch black
    'mantle':   '#141420',   # header bg
    'crust':    '#0f0f18',   # infobar bg
    'surface0': '#2a2a44',   # widget bg
    'surface1': '#3c3c60',   # hover
    'surface2': '#505080',   # active / borders
    'overlay0': '#a0a0cc',   # dim text — bright enough to read
    'overlay1': '#c0c0e8',   # secondary text
    'text':     '#ffffff',   # pure white
    'subtext0': '#e0e0f8',   # secondary text — still clearly readable
    'blue':     '#7ab4ff',
    'green':    '#90ee88',
    'mauve':    '#d084ff',   # main accent — bright purple
    'peach':    '#ffb474',
    'yellow':   '#ffe484',
    'red':      '#ff6e88',
    'teal':     '#4eecd4',   # bash accent — bright teal
    'sky':      '#5ce4f4',
    'lavender': '#a0b4ff',
    'pink':     '#ff94d4',
}

# More saturated ANSI palette — readable on bad screens
_ANSI16 = [
    '#2a2a44',  # 0  black  → surface0
    '#ff6e88',  # 1  red
    '#90ee88',  # 2  green
    '#ffe484',  # 3  yellow
    '#7ab4ff',  # 4  blue
    '#d084ff',  # 5  magenta
    '#4eecd4',  # 6  cyan
    '#ffffff',  # 7  white  → pure white
    '#505080',  # 8  bright black
    '#ff6e88',  # 9  bright red
    '#90ee88',  # 10 bright green
    '#ffe484',  # 11 bright yellow
    '#7ab4ff',  # 12 bright blue
    '#d084ff',  # 13 bright magenta
    '#4eecd4',  # 14 bright cyan
    '#ffffff',  # 15 bright white
]

_NAMED = {
    'black': 0, 'red': 1, 'green': 2, 'yellow': 3,
    'blue': 4, 'magenta': 5, 'cyan': 6, 'white': 7,
    'brightblack': 8, 'brightred': 9, 'brightgreen': 10,
    'brightyellow': 11, 'brightblue': 12, 'brightmagenta': 13,
    'brightcyan': 14, 'brightwhite': 15,
}


def _256_to_rgb(n):
    if n < 16:
        h = _ANSI16[n].lstrip('#')
        return tuple(int(h[i:i+2], 16) for i in (0, 2, 4))
    if n < 232:
        n -= 16
        r, g, b = n // 36, (n // 6) % 6, n % 6
        def v(x): return 0 if x == 0 else 55 + x * 40
        return (v(r), v(g), v(b))
    x = 8 + (n - 232) * 10
    return (x, x, x)


def resolve_color(color, default_hex):
    if color is None or color == 'default':
        return QColor(default_hex)
    if isinstance(color, str):
        if color in _NAMED:
            return QColor(_ANSI16[_NAMED[color]])
        return QColor(color)
    if isinstance(color, int):
        return QColor(*_256_to_rgb(color))
    if isinstance(color, (tuple, list)) and len(color) == 3:
        return QColor(*color)
    return QColor(default_hex)


def build_stylesheet():
    return f"""
    QMainWindow, QWidget {{
        background: {C['base']};
        color: {C['text']};
    }}
    QComboBox {{
        background: {C['surface0']};
        color: {C['text']};
        border: 1px solid {C['surface2']};
        border-radius: 4px;
        padding: 3px 10px;
        font-family: 'JetBrainsMono Nerd Font';
        font-size: 11px;
    }}
    QComboBox::drop-down {{ border: none; width: 18px; }}
    QComboBox:hover {{ border-color: {C['mauve']}; }}
    QComboBox QAbstractItemView {{
        background: {C['surface0']};
        color: {C['text']};
        selection-background-color: {C['surface1']};
        border: 1px solid {C['surface2']};
        outline: none;
    }}
    QSplitter::handle {{
        background: {C['surface0']};
    }}
    QSplitter::handle:hover {{
        background: {C['mauve']};
    }}
    QLabel {{
        color: {C['text']};
        background: transparent;
    }}
    QPushButton {{
        background: {C['surface0']};
        color: {C['text']};
        border: 1px solid {C['surface2']};
        border-radius: 4px;
        padding: 4px 10px;
        font-family: 'JetBrainsMono Nerd Font';
        font-size: 11px;
    }}
    QPushButton:hover {{
        background: {C['surface1']};
        border-color: {C['mauve']};
    }}
    """
