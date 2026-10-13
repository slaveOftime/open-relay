// Benign, network-free alternate-screen TUI. Each decoded wheel tick moves
// the visible page by one row; clicks, releases, positions and text are observable.
process.stdin.setRawMode(true);
let pending = Buffer.alloc(0);
let firstRow = 40;
let wheelEvents = 0;
let clickEvents = 0;
let releaseEvents = 0;
let invalidEvents = 0;
let held = null;
let lastPosition = '';
let order = '';
let text = Buffer.alloc(0);
let mode = process.argv[2] === 'legacy' ? 'legacy' : 'sgr';

function setMode(next) {
  mode = next;
  process.stdout.write('\x1b[?1000l\x1b[?1006l');
  if (mode !== 'off') process.stdout.write('\x1b[?1000h');
  if (mode === 'sgr') process.stdout.write('\x1b[?1006h');
}

function redraw() {
  process.stdout.write(
    '\x1b[H\x1b[0J' +
    `FIRST_HISTORY_ROW_${String(firstRow).padStart(3, '0')}\r\n` +
    `WHEEL_EVENTS_${String(wheelEvents).padStart(5, '0')}\r\n` +
    `CLICK_EVENTS_${String(clickEvents).padStart(5, '0')}\r\n` +
    `RELEASE_EVENTS_${String(releaseEvents).padStart(5, '0')}\r\n` +
    `INVALID_EVENTS_${String(invalidEvents).padStart(5, '0')}\r\n` +
    `MODE_${mode}\r\n` +
    `LAST_POSITION_${lastPosition}\r\n` +
    `ORDER_${order}\r\n` +
    `TEXT_HEX_${text.toString('hex')}\r\n` +
    Array.from({ length: 5 }, (_, index) => `HISTORY_${firstRow + index}`).join('\r\n')
  );
}

function consume(button, col, row, suffix) {
  if (suffix === 'm' || button === 3) {
    if (!held || held.col !== col || held.row !== row ||
        (mode === 'sgr' && held.button !== button)) {
      invalidEvents++;
    }
    held = null;
    releaseEvents++;
    order += 'r';
  } else if (button === 64 || button === 65) {
    lastPosition = `${col}_${row}`;
    firstRow = Math.max(1, Math.min(80, firstRow + (button === 64 ? -1 : 1)));
    wheelEvents++;
    order += button === 64 ? 'U' : 'D';
  } else if (button >= 0 && button <= 2) {
    if (held) invalidEvents++;
    held = { button, col, row };
    lastPosition = `${col}_${row}`;
    clickEvents++;
    order += ['L', 'M', 'R'][button];
  } else {
    invalidEvents++;
  }
}

process.stdout.write('\x1b[?1049h');
setMode(mode);
redraw();
process.stdin.on('data', chunk => {
  pending = Buffer.concat([pending, chunk]);
  while (pending.length) {
    if (pending[0] === 0x1b) {
      if (pending.length < 3) break;
      if (pending[1] === 0x5b && pending[2] === 0x4d) {
        if (pending.length < 6) break;
        if (mode === 'legacy') consume(pending[3] - 32, pending[4] - 32, pending[5] - 32, 'M');
        pending = pending.subarray(6);
        continue;
      }
      if (pending[1] === 0x5b && pending[2] === 0x3c) {
        const match = /^\x1b\[<(\d+);(\d+);(\d+)([mM])/.exec(pending.toString('ascii'));
        if (!match) break;
        if (mode === 'sgr') consume(Number(match[1]), Number(match[2]), Number(match[3]), match[4]);
        pending = pending.subarray(match[0].length);
        continue;
      }
    }
    const byte = pending[0];
    pending = pending.subarray(1);
    if (byte === 0x7e) setMode('off'); // ~
    else if (byte === 0x5e) setMode('sgr'); // ^ (leave @ literal for position text)
    else if (byte === 0x23) setMode('legacy'); // #
    else {
      text = Buffer.concat([text, Buffer.from([byte])]);
      order += String.fromCharCode(byte);
    }
  }
  redraw();
});
