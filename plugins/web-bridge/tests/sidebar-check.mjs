// Attach at a realistic phone width and assert comux's sidebar is NOT in the frame.
const [token, cols] = [process.argv[2], Number(process.argv[3] || 40)];
const ws = new WebSocket("ws://127.0.0.1:7575/ws/board/attach", ["bearer." + token]);
ws.binaryType = "arraybuffer";
const dec = new TextDecoder();
let buf = "";
ws.onopen = () => ws.send(JSON.stringify({ type: "resize", rows: 20, cols }));
ws.onmessage = (m) => { buf += dec.decode(new Uint8Array(m.data), { stream: true }); };
setTimeout(() => {
  const plain = buf.replace(/\x1b\[[0-9;?]*[a-zA-Z]/g, "");
  // The sidebar's two section headers. comux renders them only when it is visible.
  const hasSpaces = /\bspaces\b/.test(plain);
  const hasAgents = /\bagents\b/.test(plain);
  console.log(`cols=${cols}: sidebar "spaces"=${hasSpaces}  "agents"=${hasAgents}`);
  console.log(hasSpaces || hasAgents ? "  SIDEBAR PRESENT" : "  sidebar hidden");
  ws.close(); setTimeout(() => process.exit(hasSpaces || hasAgents ? 1 : 0), 200);
}, 5000);
