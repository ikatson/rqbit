
export const generateMagnetLink = (
    info_hash: string,
    name: string,
    trackers?: string[]
): string => {
    // 1. Uppercase info hash
    const upperHash = info_hash.toUpperCase();

    // 2. Custom encode function for lowercase hex
    const encode = (str: string): string => {
        return encodeURIComponent(str).replace(/%[0-9A-F]{2}/g, (match) => match.toLowerCase());
    }

    let magnet = `magnet:?xt=urn:btih:${upperHash}`;
    
    if (name) {
        magnet += `&dn=${encode(name)}`;
    }

    if (trackers && trackers.length > 0) {
        const trackersQuery = trackers.map(t => `tr=${encode(t)}`).join('&');
        magnet += `&${trackersQuery}`;
    }

    return magnet;
};
