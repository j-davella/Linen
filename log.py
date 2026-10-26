import logging
import sys
from pathlib import Path
from datetime import datetime

_LOG_DIR = Path.home() / '.linen' / 'logs'
_LOG_DIR.mkdir(parents=True, exist_ok=True)

_log_file = _LOG_DIR / f'{datetime.now().strftime("%Y%m%d_%H%M%S")}.log'

logging.basicConfig(
    level=logging.DEBUG,
    format='%(asctime)s [%(levelname)s] %(name)s: %(message)s',
    handlers=[
        logging.FileHandler(_log_file),
        logging.StreamHandler(sys.stderr),
    ],
)

def get(name: str) -> logging.Logger:
    return logging.getLogger(name)

log_path = _log_file
