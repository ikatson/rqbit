import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ErrorWithLabel } from "../rqbit-web";
import { ErrorComponent } from "./ErrorComponent";
import { Form } from "react-bootstrap";

interface LogStreamProps {
  httpApiBase: string;
  maxLines?: number;
}

interface Line {
  id: number;
  content: string;
  parsed: JSONLogLine;
  show: boolean;
}

const mergeBuffers = (a1: Uint8Array, a2: Uint8Array): Uint8Array => {
  const merged = new Uint8Array(a1.length + a2.length);
  merged.set(a1);
  merged.set(a2, a1.length);
  return merged;
};

const streamLogs = (
  httpApiBase: string,
  addLine: React.MutableRefObject<(text: string) => void>,
  setError: (error: ErrorWithLabel | null) => void
): (() => void) => {
  const controller = new AbortController();
  const signal = controller.signal;

  let canceled = false;

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
        addLine.current(line);
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

const LogLine = ({ line }: { line: Line }) => {
  const parsed = line.parsed;

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
              <span key={name}>
                {name} = {value}
              </span>
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
    <p
      hidden={!line.show}
      className="font-monospace m-0 text-break"
      style={{ fontSize: "10px" }}
    >
      <span className="m-1">{parsed.timestamp}</span>
      <span className={`m-1 ${classNameByLevel(parsed.level)}`}>
        {parsed.level}
      </span>

      <span className="m-1">
        {parsed.spans?.map((span, i) => (
          <span key={i}>
            <span className="fw-bold">{span.name}</span>
            {spanFields(span)}:
          </span>
        ))}
      </span>
      <span className="m-1 text-muted">{parsed.target}</span>
      <span
        className={`m-1 ${
          parsed.fields.message.match(/error|fail/g)
            ? "text-danger"
            : "text-muted"
        }`}
      >
        {parsed.fields.message}
        {Object.entries(parsed.fields)
          .filter(([key, value]) => key != "message")
          .map(([key, value]) => (
            <span className="m-1" key={key}>
              <span className="fst-italic fw-bold">{key}</span>={value}
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
  const [filter, setFilter] = useState<string>("");
  const filterRegex = useRef(new RegExp(""));

  const maxL = maxLines ?? 1000;

  const addLine = useCallback(
    (text: string) => {
      setLogLines((logLines: Line[]) => {
        const nextLineId = logLines.length == 0 ? 0 : logLines[0].id + 1;

        let newLogLines = [
          {
            id: nextLineId,
            content: text,
            parsed: JSON.parse(text) as JSONLogLine,
            show: !!text.match(filterRegex.current),
          },
          ...logLines.slice(0, maxL - 1),
        ];
        return newLogLines;
      });
    },
    [filterRegex.current, maxLines]
  );

  const addLineRef = useRef(addLine);
  addLineRef.current = addLine;

  const handleFilterChange = (value: string) => {
    setFilter(value);
    try {
      let regex = new RegExp(value);
      filterRegex.current = regex;
      setLogLines((logLines) => {
        let tmp = [...logLines];
        for (let line of tmp) {
          line.show = !!line.content.match(regex);
        }
        return tmp;
      });
    } catch (e) {}
  };

  useEffect(() => {
    return streamLogs(httpApiBase, addLineRef, setError);
  }, [httpApiBase]);

  return (
    <div className="row">
      <ErrorComponent error={error} />
      <div className="mb-3">
        Showing last {maxL} logs since this window was opened
      </div>
      <Form>
        <Form.Group className="mb-3">
          <Form.Control
            type="text"
            value={filter}
            placeholder="Enter filter (regex)"
            onChange={(e) => handleFilterChange(e.target.value)}
          />
        </Form.Group>
      </Form>

      {logLines.map((line) => (
        <LogLine key={line.id} line={line} />
      ))}
    </div>
  );
};
