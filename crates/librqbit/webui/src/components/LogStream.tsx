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

type Value = string | number | boolean;

interface Span {
  name: string;
  [key: string]: Value;
}

interface JSONLogLine {
  level: string;
  timestamp: string;
  fields: {
    message: string;
    [key: string]: Value;
  };
  target: string;
  span: Span;
  spans: Span[];
}

const EXAMPLE_LOG_JSON: JSONLogLine = {
  timestamp: "2023-12-08T21:48:13.649165Z",
  level: "DEBUG",
  fields: { message: "successfully port forwarded 192.168.0.112:4225" },
  target: "librqbit_upnp",
  span: { port: 4225, name: "manage_port" },
  spans: [
    { port: 4225, name: "upnp_forward" },
    {
      location: "http://192.168.0.1:49152/IGDdevicedesc_brlan0.xml",
      name: "upnp_endpoint",
    },
    { device: "ARRIS TG3492LG", name: "device" },
    { device: "WANDevice:1", name: "device" },
    { device: "WANConnectionDevice:1", name: "device" },
    { url: "/upnp/control/WANIPConnection0", name: "service" },
    { port: 4225, name: "manage_port" },
  ],
};

const LogLine = ({ line }: { line: string }) => {
  let parsed: JSONLogLine = JSON.parse(line);

  const classNameByLevel = (level: string) => {
    switch (level) {
      case "DEBUG":
        return "text-primary";
      case "INFO":
        return "text-success";
      case "WARN":
        return "text-warning";
      case "ERROR":
        return "text-danger";
      default:
        return "text-muted";
    }
  };

  const spanFields = (span: Span) => {
    let fields = Object.entries(span).filter(([name, value]) => name != "name");
    if (fields.length == 0) {
      return null;
    }
    return (
      <>
        {"{"}
        {fields
          .map(([name, value]) => {
            return (
              <>
                {name} = {value}
              </>
            );
          })
          .reduce((prev, curr) => (
            <>
              {prev}, {curr}
            </>
          ))}
        {"}"}
      </>
    );
  };

  return (
    <p className="font-monospace m-0 text-break" style={{ fontSize: "10px" }}>
      <span className="m-1">{parsed.timestamp}</span>
      <span className={`m-1 ${classNameByLevel(parsed.level)}`}>
        {parsed.level}
      </span>

      <span className="fw-bold m-1">
        {parsed.spans.map((span, i) => (
          <span key={i}>
            {span.name}
            {spanFields(span)}:
          </span>
        ))}
      </span>
      <span className="m-1 text-muted">{parsed.target}</span>
      <span
        className={`m-1 ${
          parsed.fields.message.match(/error|fail/g) ? "text-danger" : ""
        }`}
      >
        {parsed.fields.message}
        {Object.entries(parsed.fields)
          .filter(([key, value]) => key != "message")
          .map(([key, value]) => (
            <span className="m-1">
              {key}={value}
            </span>
          ))}
      </span>
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
