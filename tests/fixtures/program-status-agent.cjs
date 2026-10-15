// Benign pi-style OSC 7501 producer for the Windows ConPTY environment policy.
// No model/network/dependency on pi: honor its documented explicit opt-in, then
// keep the process interactive so every state can be observed before exit.
const enabled = process.env.PI_PROGRAM_STATUS === '1';
if (!process.stdin.isTTY || !process.stdout.isTTY) {
  throw new Error('program-status fixture requires a real PTY');
}
process.stdin.setRawMode(true);
process.stdout.write(`STATUS-AGENT-READY override=${process.env.PI_PROGRAM_STATUS}\r\n`);
function report(state) {
  if (enabled) process.stdout.write(`\x1b]7501;${state}\x1b\\`);
}
report('state=idle:app=pi-fixture');
const reports = [
  'state=working:app=pi-fixture',
  'state=blocked:app=pi-fixture:kind=permission:msg=QXBwcm92ZT8=',
  'state=error:app=pi-fixture',
  'state=idle:app=pi-fixture',
  'state=done:app=pi-fixture',
];
process.stdin.on('data', (data) => {
  for (const byte of data) {
    if (byte !== 120) continue; // Only the parent's x advances this fixture.
    if (!reports.length) process.exit(0);
    report(reports.shift());
  }
});
