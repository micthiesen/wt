import { createInterface } from "node:readline/promises";

export function isInteractive(): boolean {
  return Boolean(process.stdin.isTTY && process.stdout.isTTY);
}

export async function ask(question: string): Promise<string> {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    return (await rl.question(question)).trim();
  } finally {
    rl.close();
  }
}

export async function confirm(question: string, defaultYes = false): Promise<boolean> {
  const suffix = defaultYes ? " [Y/n] " : " [y/N] ";
  const answer = (await ask(question + suffix)).toLowerCase();
  if (!answer) return defaultYes;
  return answer === "y" || answer === "yes";
}

export async function pickIndex(
  items: readonly string[],
  title: string,
): Promise<number | null> {
  if (items.length === 0) return null;
  console.log(title);
  items.forEach((it, i) => console.log(`  ${i + 1}. ${it}`));
  const raw = await ask(`Pick 1-${items.length} (empty to cancel): `);
  if (!raw) return null;
  const n = parseInt(raw, 10);
  if (Number.isNaN(n) || n < 1 || n > items.length) return null;
  return n - 1;
}
