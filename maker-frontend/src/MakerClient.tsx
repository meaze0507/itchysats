import { HttpError } from "./components/HttpError";

export interface CfdNewOfferParamsPayload {
    price_short?: number;
    price_long?: number;
    min_quantity: number;
    max_quantity: number;
    daily_funding_rate_long: number;
    daily_funding_rate_short: number;
    tx_fee_rate: number;
    opening_fee?: number;
}

export async function putCfdNewOfferParamsRequest(payload: CfdNewOfferParamsPayload) {
    let res = await fetch(`/api/offer`, {
        method: "PUT",
        body: JSON.stringify(payload),
        headers: {
            "Content-Type": "application/json",
        },
        credentials: "include",
    });

    if (!res.status.toString().startsWith("2")) {
        console.log("Status: " + res.status + ", " + res.statusText);
        const resp = await res.json();
        throw new HttpError(resp);
    }
}
