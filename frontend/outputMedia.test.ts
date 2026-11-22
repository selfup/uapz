import { describe, expect, it } from "vitest";
import {
  errorText,
  guessImageMediaType,
  isImageOutput,
  outputMediaType,
} from "./outputMedia";

describe("outputMedia", () => {
  it("guesses image media types from labels", () => {
    expect(guessImageMediaType("photo.jpg")).toBe("image/jpeg");
    expect(guessImageMediaType("shot.jpeg")).toBe("image/jpeg");
    expect(guessImageMediaType("art.webp")).toBe("image/webp");
    expect(guessImageMediaType("diagram.png")).toBe("image/png");
    expect(guessImageMediaType(undefined)).toBe("image/png");
  });

  it("prefers an explicit media type", () => {
    expect(outputMediaType({ media_type: "image/gif", label: "x.jpg" })).toBe(
      "image/gif",
    );
    expect(outputMediaType({ label: "x.webp" })).toBe("image/webp");
  });

  it("detects inline image payloads", () => {
    expect(
      isImageOutput(
        { base64: "abc", media_type: "image/png", label: "out" },
        "image/png",
      ),
    ).toBe(true);
    expect(
      isImageOutput(
        { base64: "abc", media_type: "", label: "photo.jpeg" },
        "image/jpeg",
      ),
    ).toBe(true);
    expect(
      isImageOutput(
        { base64: "abc", media_type: "application/octet-stream", label: "bin" },
        "application/octet-stream",
      ),
    ).toBe(false);
    expect(isImageOutput(null, "image/png")).toBe(false);
  });

  it("stringifies unknown errors", () => {
    expect(errorText(new Error("boom"))).toBe("boom");
    expect(errorText("plain")).toBe("plain");
  });
});
