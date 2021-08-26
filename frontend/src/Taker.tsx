import { Box, Button, Center, Flex, HStack, SimpleGrid, StackDivider, Text, useToast, VStack } from "@chakra-ui/react";
import axios from "axios";
import React, { useState } from "react";
import { useAsync } from "react-async";
import { Route, Routes } from "react-router-dom";
import { useEventSource } from "react-sse-hooks";
import "./App.css";
import CfdTile from "./components/CfdTile";
import CurrencyInputField from "./components/CurrencyInputField";
import useLatestEvent from "./components/Hooks";
import NavLink from "./components/NavLink";
import { Cfd, Offer } from "./components/Types";

/* TODO: Change from localhost:8000 */
const BASE_URL = "http://localhost:8000";

interface CfdTakeRequestPayload {
    offer_id: string;
    quantity: number;
}

async function postCfdTakeRequest(payload: CfdTakeRequestPayload) {
    let res = await axios.post(BASE_URL + `/cfd`, JSON.stringify(payload));

    if (!res.status.toString().startsWith("2")) {
        throw new Error("failed to create new CFD take request: " + res.status + ", " + res.statusText);
    }
}

export default function App() {
    let source = useEventSource({ source: BASE_URL + "/feed" });

    const cfds = useLatestEvent<Cfd[]>(source, "cfds");
    const offer = useLatestEvent<Offer>(source, "offer");
    const balance = useLatestEvent<number>(source, "balance");

    const toast = useToast();
    let [quantity, setQuantity] = useState<string>("10000");
    const format = (val: any) => `$` + val;
    const parse = (val: any) => val.replace(/^\$/, "");

    let { run: makeNewTakeRequest, isLoading: isCreatingNewTakeRequest } = useAsync({
        deferFn: async ([payload]: any[]) => {
            try {
                await postCfdTakeRequest(payload as CfdTakeRequestPayload);
            } catch (e) {
                const description = typeof e === "string" ? e : JSON.stringify(e);

                toast({
                    title: "Error",
                    description,
                    status: "error",
                    duration: 9000,
                    isClosable: true,
                });
            }
        },
    });

    return (
        <Center marginTop={50}>
            <HStack>
                <Box marginRight={5}>
                    <VStack align={"top"}>
                        <NavLink text={"trade"} path={"trade"} />
                        <NavLink text={"wallet"} path={"wallet"} />
                        <NavLink text={"settings"} path={"settings"} />
                    </VStack>
                </Box>
                <Box width={1200} height="100%" maxHeight={800}>
                    <Routes>
                        <Route path="trade">
                            <Flex direction={"row"} height={"100%"}>
                                <Flex direction={"row"} width={"100%"}>
                                    <VStack
                                        spacing={5}
                                        shadow={"md"}
                                        padding={5}
                                        width={"100%"}
                                        divider={<StackDivider borderColor="gray.200" />}
                                    >
                                        <Box width={"100%"} overflow={"scroll"}>
                                            <SimpleGrid columns={2} spacing={10}>
                                                {cfds && cfds.map((cfd, index) =>
                                                    <CfdTile
                                                        key={"cfd_" + index}
                                                        index={index}
                                                        cfd={cfd}
                                                    />
                                                )}
                                            </SimpleGrid>
                                        </Box>
                                    </VStack>
                                </Flex>
                                <Flex width={"50%"} marginLeft={5}>
                                    <VStack spacing={5} shadow={"md"} padding={5} align={"stretch"}>
                                        <HStack>
                                            <Text align={"left"}>Your balance:</Text>
                                            <Text>{balance}</Text>
                                        </HStack>
                                        <HStack>
                                            {/*TODO: Do we need this? does it make sense to only display the price from the offer?*/}
                                            <Text align={"left"}>Current Price (Kraken):</Text>
                                            <Text>tbd</Text>
                                        </HStack>
                                        <HStack>
                                            <Text align={"left"}>Offer Price:</Text>
                                            <Text>{offer?.price}</Text>
                                        </HStack>
                                        <HStack>
                                            <Text>Quantity:</Text>
                                            <CurrencyInputField
                                                onChange={(valueString: string) => setQuantity(parse(valueString))}
                                                value={format(quantity)}
                                            />
                                        </HStack>
                                        <Text>Leverage:</Text>
                                        {/* TODO: consider button group */}
                                        <Flex justifyContent={"space-between"}>
                                            <Button disabled={true}>x1</Button>
                                            <Button disabled={true}>x2</Button>
                                            <Button colorScheme="blue" variant="solid">x{offer?.leverage}</Button>
                                        </Flex>
                                        {<Button
                                            disabled={isCreatingNewTakeRequest || !offer}
                                            variant={"solid"}
                                            colorScheme={"blue"}
                                            onClick={() => {
                                                let payload: CfdTakeRequestPayload = {
                                                    offer_id: offer!.id,
                                                    quantity: Number.parseFloat(quantity),
                                                };
                                                makeNewTakeRequest(payload);
                                            }}
                                        >
                                            BUY
                                        </Button>}
                                    </VStack>
                                </Flex>
                            </Flex>
                        </Route>
                        <Route path="wallet">
                            <Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Wallet</Text>
                                </Box>
                            </Center>
                        </Route>
                        <Route path="settings">
                            <Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Settings</Text>
                                </Box>
                            </Center>
                        </Route>
                    </Routes>
                </Box>
            </HStack>
        </Center>
    );
}
