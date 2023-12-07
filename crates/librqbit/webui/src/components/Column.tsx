import { Col } from "react-bootstrap";

export const Column: React.FC<{
  label: string;
  size?: number;
  children?: any;
}> = ({ size, label, children }) => (
  <Col md={size || 1} className="py-3">
    <div className="fw-bold">{label}</div>
    {children}
  </Col>
);
