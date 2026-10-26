from pathlib import Path

DATA_DIR = Path.home() / '.linen'

CLAUDE_CMD  = ['/run/current-system/sw/bin/claude', '--dangerously-skip-permissions']
BASH_CMD    = ['/run/current-system/sw/bin/bash']

FONT_FAMILY = 'JetBrainsMono Nerd Font'
FONT_SIZE   = 11
SCROLLBACK  = 2000

MODELS = [
    'claude-sonnet-4-6',
    'claude-opus-4-7',
    'claude-haiku-4-5-20251001',
]
DEFAULT_MODEL = 'claude-sonnet-4-6'
