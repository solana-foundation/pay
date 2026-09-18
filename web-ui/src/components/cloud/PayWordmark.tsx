import wordmark from "../../cloud/assets/paysh-wordmark.svg?raw";

/**
 * The pay.sh pixel wordmark from pay-web-ui, inlined so it takes the
 * current text colour. Same asset as the marketing site's hero.
 */
export function PayWordmark() {
  return (
    <div
      className="cloud-term-wordmark"
      role="img"
      aria-label="pay.sh"
      dangerouslySetInnerHTML={{ __html: wordmark }}
    />
  );
}
