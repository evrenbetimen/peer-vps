const MICROS = 1_000_000;

export const credits = (micros: number, digits = 2) =>
  (micros / MICROS).toLocaleString(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits });

export const perHour = (microsPerSec: number) => `${credits(microsPerSec * 3600)} cr/h`;

export const mib = (n: number) => (n >= 1024 ? `${(n / 1024).toFixed(n % 1024 ? 1 : 0)} GiB` : `${n} MiB`);

export function bytesPerSec(bps: number) {
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let v = bps;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i += 1;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export const time = (unixSecs: number) => new Date(unixSecs * 1000).toLocaleTimeString();

export const cx = (...parts: (string | false | null | undefined)[]) => parts.filter(Boolean).join(" ");
