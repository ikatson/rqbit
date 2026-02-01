
import { useState } from "react";
import { Button } from "./Button";
import { CreateTorrentModal } from "../modal/CreateTorrentModal";

export const CreateTorrentButton = ({ className }: { className?: string }) => {
    const [showModal, setShowModal] = useState(false);

    return (
        <>
            <Button onClick={() => setShowModal(true)} className={className}>
                Create Torrent
            </Button>
            {showModal && <CreateTorrentModal onHide={() => setShowModal(false)} />}
        </>
    );
};
