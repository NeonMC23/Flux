import os, sys, time, re, codecs
import pexpect

class Screen:
    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[' ']*cols for _ in range(rows)]
        self.r = self.c = 0
    def clear(self):
        self.grid = [[' ']*self.cols for _ in range(self.rows)]
        self.r = self.c = 0
    def put(self, ch):
        w = 2 if __import__('unicodedata').east_asian_width(ch) in 'WF' else 1
        if 0 <= self.r < self.rows and 0 <= self.c < self.cols:
            self.grid[self.r][self.c] = ch
            if w == 2 and self.c+1 < self.cols:
                self.grid[self.r][self.c+1] = ''
        self.c += w
        if self.c >= self.cols:
            self.c = self.cols-1
    def feed(self, text):
        i = 0
        n = len(text)
        while i < n:
            ch = text[i]
            if ch == '\x1b':
                if i+1 < n and text[i+1] == '[':
                    j = i+2
                    while j < n and not ('@' <= text[j] <= '~'):
                        j += 1
                    if j >= n: break
                    body = text[i+2:j]
                    final = text[j]
                    self.csi(body, final)
                    i = j+1
                    continue
                elif i+1 < n and text[i+1] == ']':
                    j = text.find('\x07', i)
                    if j == -1: break
                    i = j+1
                    continue
                else:
                    i += 2
                    continue
            elif ch == '\r': self.c = 0
            elif ch == '\n':
                self.r = min(self.r+1, self.rows-1); self.c = 0
            elif ch == '\b': self.c = max(0, self.c-1)
            elif ch >= ' ' and ch != '\x7f': self.put(ch)
            i += 1
    def csi(self, body, final):
        private = body.startswith('?')
        if private: body = body[1:]
        parts = [p for p in body.split(';') if p != '']
        try: nums = [int(p) for p in parts]
        except ValueError: nums = []
        if final in 'Hf':
            r = (nums[0]-1) if len(nums) >= 1 else 0
            c = (nums[1]-1) if len(nums) >= 2 else 0
            self.r = max(0, min(r, self.rows-1)); self.c = max(0, min(c, self.cols-1))
        elif final == 'A': self.r = max(0, self.r-(nums[0] if nums else 1))
        elif final == 'B': self.r = min(self.rows-1, self.r+(nums[0] if nums else 1))
        elif final == 'C': self.c = min(self.cols-1, self.c+(nums[0] if nums else 1))
        elif final == 'D': self.c = max(0, self.c-(nums[0] if nums else 1))
        elif final == 'G': self.c = max(0, min((nums[0]-1) if nums else 0, self.cols-1))
        elif final == 'd': self.r = max(0, min((nums[0]-1) if nums else 0, self.rows-1))
        elif final == 'J':
            mode = nums[0] if nums else 0
            if mode == 2 or mode == 3: self.clear()
            else:
                for c in range(self.c, self.cols): self.grid[self.r][c] = ' '
                for r in range(self.r+1, self.rows):
                    self.grid[r] = [' ']*self.cols
        elif final == 'K':
            mode = nums[0] if nums else 0
            if mode == 0:
                for c in range(self.c, self.cols): self.grid[self.r][c] = ' '
            elif mode == 1:
                for c in range(0, self.c+1): self.grid[self.r][c] = ' '
            else:
                self.grid[self.r] = [' ']*self.cols
        elif final == 'm':
            pass
        elif final in 'hl' and private:
            if body == '1049' and final == 'h': self.clear()
    def text(self):
        return '\n'.join(''.join(row).rstrip() for row in self.grid)

def drain(child, screen, seconds=0.6):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            data = child.read_nonblocking(size=65536, timeout=0.1)
        except Exception:
            continue
        if data:
            screen.feed(data)

def capture(argv, env, rows, cols, actions, settle=0.7):
    child = pexpect.spawn(argv[0], argv[1:], env=env, dimensions=(rows, cols), encoding='utf-8', timeout=10, echo=False)
    screen = Screen(rows, cols)
    frames = []
    time.sleep(2.0)
    drain(child, screen, 0.8)
    frames.append(('startup', screen.text()))
    for label, keys in actions:
        child.send(keys)
        time.sleep(settle)
        drain(child, screen, 0.6)
        frames.append((label, screen.text()))
    child.send('q')
    time.sleep(0.4)
    drain(child, screen, 0.4)
    child.close(force=True)
    return frames

if __name__ == '__main__':
    if len(sys.argv) != 5:
        print('usage: tui_pty_capture.py <flux-tui-binary> <rows> <cols> <output-dir>')
        raise SystemExit(2)
    binary = sys.argv[1]
    rows, cols = int(sys.argv[2]), int(sys.argv[3])
    outdir = sys.argv[4]
    os.makedirs(outdir, exist_ok=True)
    env = dict(os.environ)
    env.pop('WAYLAND_DISPLAY', None)
    env['TERM'] = 'xterm-256color'
    env.setdefault('HOME', os.path.expanduser('~'))
    env['FLUX_TUI_CONFIG'] = os.path.join(outdir, 'tui.conf')
    actions = [
        ('startup-standard', ''),
        ('track-move', '\x1bm'),
        ('move-cancel', '\x1b'),
        ('track-delete', '\x1b[3~'),
        ('delete-cancel', '\x1b'),
        ('layout-library', 'm'),
        ('layout-player', 'm'),
        ('layout-visualizer', 'm'),
        ('layout-compact', 'm'),
        ('layout-standard', 'm'),
        ('settings', '\x1b[18~'),
        ('artists', '\x1bOQ'),
        ('artist-detail', '\r'),
        ('tracks', '\x1bOP'),
        ('track-form', 'e'),
        ('form-cancel', '\x1b'),
        ('help', '?'),
        ('back', '\x1b'),
    ]
    for label, text in capture([binary], env, rows, cols, actions):
        with open(os.path.join(outdir, f'{label}.txt'), 'w', encoding='utf-8') as f:
            f.write(text + '\n')
    print('captured', outdir)
