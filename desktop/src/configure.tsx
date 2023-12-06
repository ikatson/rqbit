import React, { useState } from "react";
import { RqbitDesktopConfig } from "./configuration";
import { Button, Form, Modal, Row, Tab, Tabs } from "react-bootstrap";
import { ErrorComponent } from "./rqbit-webui-src/rqbit-web";
import { invokeAPI } from "./api";
import { ErrorDetails } from "./rqbit-webui-src/api-types";

const FormCheck: React.FC<{
    label: string,
    name: string,
    checked: boolean,
    onChange: (e: any) => void,
    disabled?: boolean,
    help?: string,
}> = ({ label, name, checked, onChange, disabled, help }) => {
    return <Form.Group as={Row} controlId={name} className="mb-3">
        <Form.Label className="col-4">{label}</Form.Label>
        <div className="col-8">
            <Form.Check
                type="switch"
                name={name}
                checked={checked}
                onChange={onChange}
                disabled={disabled}
            />
        </div>
        {help && <div className="form-text">{help}</div>}
    </Form.Group>
}

const FormInput: React.FC<{
    label: string,
    name: string,
    value: string | number,
    inputType: string,
    onChange: (e: any) => void,
    disabled?: boolean,
    help?: string
}> = ({ label, name, value, inputType, onChange, disabled, help }) => {
    return <Form.Group as={Row} controlId={name} className="mb-3">
        <Form.Label className="col-4 col-form-label">{label}</Form.Label>
        <div className="col-8">
            <Form.Control
                type={inputType}
                name={name}
                value={value}
                onChange={onChange}
                disabled={disabled}
            />
        </div>
        {help && <div className="form-text">{help}</div>}
    </Form.Group>
}

export const ConfigModal: React.FC<{
    show: boolean,
    handleStartReconfigure: () => void,
    handleConfigured: (config: RqbitDesktopConfig) => void,
    handleCancel?: () => void,
    initialConfig: RqbitDesktopConfig,
    defaultConfig: RqbitDesktopConfig,
}> = ({ show, handleStartReconfigure, handleConfigured, handleCancel, initialConfig, defaultConfig }) => {
    let [config, setConfig] = useState<RqbitDesktopConfig>(initialConfig);
    let [loading, setLoading] = useState<boolean>(false);

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
        handleStartReconfigure();
        setLoading(true);
        invokeAPI<{}>("config_change", { config }).then(
            () => {
                setLoading(false);
                handleConfigured(config);
            },
            (e: ErrorDetails) => {
                setLoading(false);
                setError({
                    text: "Error saving configuration",
                    details: e,
                });
            }
        )
    };

    return (
        <Modal show={show} size='xl' onHide={handleCancel}>
            <Modal.Header closeButton>
                <Modal.Title>Configure Rqbit desktop</Modal.Title>
            </Modal.Header>
            <Modal.Body>
                <ErrorComponent error={error}></ErrorComponent>
                <Tabs
                    defaultActiveKey="home"
                    id="rqbit-config"
                    className="mb-3">

                    <Tab className="mb-3" eventKey="home" title="Home">
                        <FormInput
                            label="Default download folder"
                            name="default_download_location"
                            value={config.default_download_location}
                            inputType="text"
                            onChange={handleInputChange}
                            help="Where to download torrents by default. You can override this per torrent."
                        />
                    </Tab>

                    <Tab className="mb-3" eventKey="dht" title="DHT">
                        <legend>DHT config</legend>

                        <FormCheck
                            label="Enable DHT"
                            name="dht.disable"
                            checked={!config.dht.disable}
                            onChange={handleToggleChange}
                            help="DHT is required to read magnet links. There's no good reason to disable it, unless you know what you are doing."
                        />

                        <FormCheck
                            label="Enable DHT persistence"
                            name="dht.disable_persistence"
                            checked={!config.dht.disable_persistence}
                            onChange={handleToggleChange}
                            help="Enable to store DHT state in a file periodically. If disabled, DHT will bootstrap from scratch on restart."
                        />

                        <FormInput
                            label="Persistence filename"
                            name="dht.persistence_filename"
                            value={config.dht.persistence_filename}
                            inputType="text"
                            onChange={handleInputChange}
                            help="The filename to store DHT state into"
                        />
                    </Tab>

                    <Tab className="mb-3" eventKey="tcp_listen" title="TCP">
                        <legend>TCP Listener config</legend>

                        <FormCheck
                            label="Listen on TCP"
                            name="tcp_listen.disable"
                            checked={!config.tcp_listen.disable}
                            onChange={handleToggleChange}
                            help="Listen for torrent requests on TCP. Required for peers to be able to connect to you, mainly for uploading."
                        />

                        <FormCheck
                            label="Advertise over UPnP"
                            name="tcp_listen.disable"
                            checked={!config.tcp_listen.disable}
                            onChange={handleToggleChange}
                            help="Advertise your port over UPnP. This is required for peers to be able to connect to you from the internet. Will only work if your router has a static IP."
                        />

                        <FormInput
                            inputType="number"
                            label="Min port"
                            name="tcp_listen.min_port"
                            value={config.tcp_listen.min_port}
                            disabled={config.tcp_listen.disable}
                            onChange={handleInputChange}
                            help="The min port to try to listen on. First successful is taken."
                        />

                        <FormInput
                            inputType="number"
                            label="Max port"
                            name="tcp_listen.max_port"
                            value={config.tcp_listen.max_port}
                            disabled={config.tcp_listen.disable}
                            onChange={handleInputChange}
                            help="The max port to try to listen on."
                        />
                    </Tab>


                    <Tab className="mb-3" eventKey="session_persistence" title="Session">
                        <legend>Session persistence</legend>

                        <FormCheck
                            label="Enable persistence"
                            name="persistence.disable"
                            checked={!config.persistence.disable}
                            onChange={handleToggleChange}
                            help="If you disable session persistence, rqbit won't remember the torrents you had before restart."
                        />

                        <FormInput
                            label="Persistence filename"
                            name="persistence.filename"
                            inputType="text"
                            value={config.persistence.filename}
                            onChange={handleInputChange}
                            disabled={config.persistence.disable}
                        />
                    </Tab>

                    <Tab className="mb-3" eventKey="peer_opts" title="Peer options">
                        <legend>Peer connection options</legend>

                        <FormInput
                            label="Connect timeout (seconds)"
                            inputType="number"
                            name="peer_opts.connect_timeout"
                            value={config.peer_opts.connect_timeout}
                            onChange={handleInputChange}
                            help="How much to wait for outgoing connections to connect. Default is low to prefer faster peers."
                        />

                        <FormInput
                            label="Read/write timeout (seconds)"
                            inputType="number"
                            name="peer_opts.read_write_timeout"
                            value={config.peer_opts.read_write_timeout}
                            onChange={handleInputChange}
                            help="Peer socket read/write timeout."
                        />
                    </Tab>

                    <Tab className="mb-3" eventKey="http_api" title="HTTP API">
                        <legend>HTTP API config</legend>

                        <FormCheck
                            label="Enable HTTP API"
                            name="http_api.disable"
                            checked={!config.http_api.disable}
                            onChange={handleToggleChange}
                            help="If enabled you can access the HTTP API at the address below"
                        />

                        <FormCheck
                            label="Read only"
                            name="http_api.read_only"
                            checked={config.http_api.read_only}
                            disabled={config.http_api.disable}
                            onChange={handleToggleChange}
                            help="If enabled, only GET requests will be allowed through the API"
                        />

                        <FormInput
                            label="Listen address"
                            inputType="text"
                            name="http_api.listen_addr"
                            value={config.http_api.listen_addr}
                            disabled={config.http_api.disable}
                            onChange={handleInputChange}
                            help={`You'll access the API at http://${config.http_api.listen_addr}`}
                        />
                    </Tab>

                </Tabs>
            </Modal.Body>
            <Modal.Footer>

                {!!handleCancel &&
                    <Button variant="secondary" onClick={handleCancel}>
                        Cancel
                    </Button>
                }
                <Button variant="secondary" onClick={() => setConfig(defaultConfig)}>
                    Reset to defaults
                </Button>
                <Button variant="primary" onClick={handleOkClick} disabled={loading}>
                    OK
                </Button>
            </Modal.Footer>
        </Modal>
    );
};
