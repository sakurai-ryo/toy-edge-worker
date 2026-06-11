// ヒープ上限のデモ: near-heap-limit コールバック経由で terminate され、
// この worker の isolate は破棄される（次のリクエストは cold start になる）
export default {
  async fetch() {
    const chunks = [];
    while (true) {
      chunks.push(new Array(1024 * 1024).fill(Math.random()));
    }
  },
};
