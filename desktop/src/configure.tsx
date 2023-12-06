import React, { useState } from "react";
import { RqbitDesktopConfig } from "./configuration";
import { Button, Form, Modal, Tab, Tabs } from "react-bootstrap";
import { ErrorComponent } from "./rqbit-webui-src/rqbit-web";
import { invokeAPI } from "./api";
import { ErrorDetails } from "./rqbit-webui-src/api-types";

export const ConfigModal: React.FC<{
    handleOk: (config: RqbitDesktopConfig) => void,
    initialConfig: RqbitDesktopConfig,
}> = ({ handleOk, initialConfig }) => {
    const [config, setConfig] = useState(initialConfig);
    const [error, setError] = useState<any | null>(null);

    const handleInputChange = (e: any) => {
        const name: string = e.target.name;
        const value: any = e.target.value;
        const [mainField, subField] = name.split('.', 2);

        if (subField) {
            setConfig((prevConfig: any) => ({
                ...prevConfig,
                [mainField]: {
                    ...prevConfig[mainField],
                    [subField]: value,
                },
            }));
        } else {
            setConfig((prevConfig) => ({
                ...prevConfig,
                [name]: value,
            }));
        }
    };

    const handleToggleChange = (e: any) => {
        const name: string = e.target.name;
        const [mainField, subField] = name.split('.', 2);

        if (subField) {
            setConfig((prevConfig: any) => ({
                ...prevConfig,
                [mainField]: {
                    ...prevConfig[mainField],
                    [subField]: !prevConfig[mainField][subField],
                },
            }));
        } else {
            setConfig((prevConfig: any) => ({
                ...prevConfig,
                [name]: !prevConfig[name],
            }));
        }
    };

    const handleOkClick = () => {
        setError(null);
        invokeAPI<{}>("config_change", { config }).then(
            () => handleOk(config),
            (e: ErrorDetails) => {
                setError({
                    text: "Error saving configuration",
                    details: e,
                });
            }
        )
    };

    return (
        <Modal show size='xl'>
            <Modal.Header closeButton>
                <Modal.Title>Configure Rqbit desktop</Modal.Title>
            </Modal.Header>
            <Modal.Body>
                <ErrorComponent error={error}></ErrorComponent>
                <Tabs
                    defaultActiveKey="main"
                    id="rqbit-config"
                    className="mb-3">

                    <Tab className="mb-3" eventKey="home" title="Home">
                        <Form.Group controlId="default_download_location">
                            <Form.Label>Default Download Location</Form.Label>
                            <Form.Control
                                type="text"
                                name="default_download_location"
                                value={config.default_download_location}
                                onChange={handleInputChange}
                            />
                        </Form.Group>
                    </Tab>

                    <Tab className="mb-3" eventKey="dht" title="DHT">
                        <legend>DHT config</legend>

                        <Form.Group controlId="dht_disable">
                            <Form.Check
                                type="switch"
                                label="Disable DHT"
                                name="dht.disable"
                                checked={config.dht.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="dht_disable_persistence">
                            <Form.Check
                                type="switch"
                                label="Disable DHT Persistence"
                                name="dht.disable_persistence"
                                checked={config.dht.disable_persistence}
                                disabled={config.dht.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="dht_persistence_filename">
                            <Form.Label>Persistence Filename</Form.Label>
                            <Form.Control
                                type="text"
                                name="dht.persistence_filename"
                                value={config.dht.persistence_filename}
                                disabled={config.dht.disable}
                                onChange={handleInputChange}
                            />
                        </Form.Group>
                    </Tab>

                    <Tab className="mb-3" eventKey="tcp_listen" title="TCP">

                        <legend>TCP Listener config</legend>

                        <Form.Group controlId="tcp_listen_disable">
                            <Form.Check
                                type="switch"
                                label="Disable TCP Listen"
                                name="tcp_listen.disable"
                                checked={config.tcp_listen.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="tcp_listen_min_port">
                            <Form.Label>Min port</Form.Label>
                            <Form.Control
                                type="number"
                                name="tcp_listen.min_port"
                                value={config.tcp_listen.min_port}
                                disabled={config.tcp_listen.disable}
                                onChange={handleInputChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="tcp_listen_max_port">
                            <Form.Label>Max Port</Form.Label>
                            <Form.Control
                                type="number"
                                name="tcp_listen.max_port"
                                value={config.tcp_listen.max_port}
                                disabled={config.tcp_listen.disable}
                                onChange={handleInputChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="upnp_disable">
                            <Form.Check
                                type="switch"
                                label="Do not advertise TCP port over UPnP"
                                name="upnp.disable"
                                checked={config.upnp.disable}
                                disabled={config.tcp_listen.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                    </Tab>


                    <Tab className="mb-3" eventKey="session_persistence" title="Session">
                        <legend>Session persistence</legend>

                        <Form.Group controlId="persistence_disable">
                            <Form.Check
                                type="switch"
                                label="Disable Persistence"
                                name="persistence.disable"
                                checked={config.persistence.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="persistence_filename">
                            <Form.Label>Persistence Filename</Form.Label>
                            <Form.Control
                                type="text"
                                name="persistence.filename"
                                value={config.persistence.filename}
                                disabled={config.persistence.disable}
                                onChange={handleInputChange}
                            />
                        </Form.Group>

                    </Tab>

                    <Tab className="mb-3" eventKey="peer_opts" title="Peer options">
                        <legend>Peer connection options</legend>

                        <Form.Group controlId="peer_opts_connect_timeout">
                            <Form.Label>Peer Options Connect Timeout</Form.Label>
                            <Form.Control
                                type="text"
                                name="peer_opts.connect_timeout"
                                value={config.peer_opts.connect_timeout}
                                onChange={handleInputChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="peer_opts_read_write_timeout">
                            <Form.Label>Peer Options Read Write Timeout</Form.Label>
                            <Form.Control
                                type="text"
                                name="peer_opts.read_write_timeout"
                                value={config.peer_opts.read_write_timeout}
                                onChange={handleInputChange}
                            />
                        </Form.Group>
                    </Tab>

                    <Tab className="mb-3" eventKey="http_api" title="HTTP API">
                        <legend>HTTP API config</legend>

                        <Form.Group controlId="http_api_disable">
                            <Form.Check
                                type="switch"
                                label="Disable HTTP API"
                                name="http_api.disable"
                                checked={config.http_api.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="http_api_listen_addr">
                            <Form.Label>HTTP API Listen Address</Form.Label>
                            <Form.Control
                                type="text"
                                name="http_api.listen_addr"
                                value={config.http_api.listen_addr}
                                disabled={config.http_api.disable}
                                onChange={handleInputChange}
                            />
                        </Form.Group>

                        <Form.Group controlId="http_api_read_only">
                            <Form.Check
                                type="switch"
                                label="HTTP API Read Only"
                                name="http_api.read_only"
                                checked={config.http_api.read_only}
                                disabled={config.http_api.disable}
                                onChange={handleToggleChange}
                            />
                        </Form.Group>

                    </Tab>

                </Tabs>
            </Modal.Body>
            <Modal.Footer>
                <Button variant="secondary" onClick={() => setConfig(initialConfig)}>
                    Reset to defaults
                </Button>
                <Button variant="primary" onClick={handleOkClick}>
                    OK
                </Button>
            </Modal.Footer>
        </Modal>
    );
};
