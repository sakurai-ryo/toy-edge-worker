// CPU 時間制限のデモ: 無限ループは watchdog の terminate_execution で殺される
export default {
  async fetch() {
    while (true) {}
  },
};
