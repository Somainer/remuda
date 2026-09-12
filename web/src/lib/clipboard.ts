export const clipboardIo = {
  write(text: string) {
    return navigator.clipboard?.writeText(text) ?? Promise.resolve();
  },
};
