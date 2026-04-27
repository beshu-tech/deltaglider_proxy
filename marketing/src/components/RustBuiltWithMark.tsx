/**
 * Official Rust mark (black) from Wikimedia Commons; dark mode: CSS invert.
 * https://commons.wikimedia.org/wiki/File:Rust_programming_language_black_logo.svg
 */
const RUST_LOGO = `${import.meta.env.BASE_URL}brand/rust-logo.svg`;

export function RustBuiltWithMark(): JSX.Element {
  return (
    <a
      href="https://www.rust-lang.org/"
      target="_blank"
      rel="noopener noreferrer"
      className="inline-flex max-w-full items-center gap-2 rounded-md text-[13px] text-ink-500 no-underline transition hover:text-ink-700 dark:text-ink-400 dark:hover:text-ink-200"
      title="DeltaGlider Proxy is written in the Rust programming language"
    >
      <img
        src={RUST_LOGO}
        alt=""
        width={20}
        height={20}
        className="h-[18px] w-[18px] shrink-0 opacity-55 dark:invert dark:opacity-65"
        loading="eager"
        decoding="async"
      />
      <span className="font-medium tracking-tight">Written in Rust & React ❤️</span>
    </a>
  );
}
