import React, { useEffect, useState } from "react";
import { ErrorWithLabel } from "../rqbit-web";
import { ErrorComponent } from "./ErrorComponent";

interface LogStreamProps {
  httpApiBase: string;
  maxLines?: number;
}

interface Line {
  id: number;
  content: string;
}

const mergeBuffers = (a1: Uint8Array, a2: Uint8Array): Uint8Array => {
  const merged = new Uint8Array(a1.length + a2.length);
  merged.set(a1);
  merged.set(a2, a1.length);
  return merged;
};

const streamLogs = (
  httpApiBase: string,
  addLine: (text: string) => void,
  setError: (error: ErrorWithLabel | null) => void
): (() => void) => {
  const controller = new AbortController();
  const signal = controller.signal;

  let canceled = true;

  const cancel = () => {
    console.log("cancelling fetch");
    canceled = true;
    controller.abort();
  };

  const run = async () => {
    let response = null;
    try {
      response = await fetch(httpApiBase + "/stream_logs", { signal });
    } catch (e: any) {
      if (canceled) {
        return;
      }
      setError({
        text: "network error fetching logs",
        details: {
          text: e.toString(),
        },
      });
      return null;
    }

    if (!response.ok) {
      let text = await response.text();
      setError({
        text: "error fetching logs",
        details: {
          statusText: response.statusText,
          text,
        },
      });
    }

    if (!response.body) {
      setError({
        text: "error fetching logs: ReadableStream not supported.",
      });
      throw new Error("ReadableStream not supported.");
    }

    const reader = response.body.getReader();

    let buffer = new Uint8Array();
    while (true) {
      const { done, value } = await reader.read();

      if (done) {
        // Handle stream completion or errors
        break;
      }

      buffer = mergeBuffers(buffer, value);

      while (true) {
        const newLineIdx = buffer.indexOf(10);
        if (newLineIdx === -1) {
          break;
        }
        let lineBytes = buffer.slice(0, newLineIdx);
        let line = new TextDecoder().decode(lineBytes);
        addLine(line);
        buffer = buffer.slice(newLineIdx + 1);
      }
    }
  };
  run();

  return cancel;
};

const SplitByLevelRegexp = new RegExp(
  /(.*?) +(INFO|WARN|TRACE|ERROR|DEBUG) +(.*)/
);

const LogLine = ({ line }: { line: string }) => {
  line.split;
  const getClassNameByLevel = (level: string) => {
    switch (level) {
      case "INFO":
        return "text-success";
      case "WARN":
        return "text-warning";
      case "ERROR":
        return "text-danger";
      case "DEBUG":
        return "text-primary";
      default:
        return "text-secondary";
    }
  };

  const getContent = () => {
    let match = line.match(SplitByLevelRegexp);
    if (!match) {
      return line;
    }
    const [beforeLevel, level, afterLevel] = match.slice(1);
    return (
      <>
        {beforeLevel}
        <span className={`${getClassNameByLevel(level)} m-2`}>{level}</span>
        {afterLevel}
      </>
    );
  };

  return (
    <p className="font-monospace m-0" style={{ fontSize: "10px" }}>
      {getContent()}
    </p>
  );
};

export const LogStream: React.FC<LogStreamProps> = ({
  httpApiBase,
  maxLines,
}) => {
  const [logLines, setLogLines] = useState<Line[]>([]);
  const [error, setError] = useState<ErrorWithLabel | null>(null);
  const maxL = maxLines ?? 1000;

  const addLine = (text: string) => {
    setLogLines((logLines: Line[]) => {
      const nextLineId = logLines.length == 0 ? 0 : logLines[0].id + 1;

      let newLogLines = [
        {
          id: nextLineId,
          content: text,
        },
        ...logLines.slice(0, maxL - 1),
      ];
      return newLogLines;
    });
  };

  useEffect(() => {
    return streamLogs(httpApiBase, addLine, setError);
  }, [httpApiBase]);

  return (
    <div className="row">
      <ErrorComponent error={error} />
      {logLines.map((line) => (
        <LogLine key={line.id} line={line.content} />
      ))}
    </div>
  );
};
