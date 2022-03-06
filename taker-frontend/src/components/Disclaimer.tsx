import { ExternalLinkIcon } from "@chakra-ui/icons";
import {
    Button,
    Checkbox,
    HStack,
    Link,
    Modal,
    ModalBody,
    ModalCloseButton,
    ModalContent,
    ModalFooter,
    ModalHeader,
    ModalOverlay,
    Text,
    useColorModeValue,
    useDisclosure,
} from "@chakra-ui/react";
import React, { useEffect, useState } from "react";

export interface DisclaimerProps {
    setHideDisclaimer: (value: boolean) => void;
}

// Disclaimer is a modal pop-up that opens up before you start using the app and
// goes away after the user dismisses it. It displays the information we want
// the user to know before they start using the software.
export default function Disclaimer({ setHideDisclaimer }: DisclaimerProps) {
    const [ack, setAck] = useState<boolean>(false);
    const [hideDisclaimerCheckbox, setHideDisclaimerCheckbox] = useState<boolean>(true);

    const { isOpen, onOpen, onClose } = useDisclosure();

    const iconColor = useColorModeValue("white.200", "white.600");

    useEffect(() => {
        if (!ack) {
            onOpen();
        } else {
            onClose();
        }
    }, [ack, onClose, onOpen]);

    return (
        <>
            <Modal isOpen={isOpen} onClose={onClose}>
                <ModalOverlay />
                <ModalContent>
                    <ModalHeader>Disclaimer</ModalHeader>
                    <ModalCloseButton />
                    <ModalBody>
                        <Text>
                            ItchySats is a new and complex system that has not been fully audited. Like most complex
                            software systems, ItchySats may contain bugs which, in extreme cases, could lead to a loss
                            of funds.
                        </Text>
                        <br />
                        <Text>Please be careful and test on testnet first following</Text>
                        <Link
                            href="https://itchysats.medium.com/p2p-bitcoin-cfds-give-it-a-try-4db2d5804328"
                            isExternal
                        >
                            this guide!
                            <ExternalLinkIcon mx="2px" color={iconColor} />
                        </Link>
                        <Text>Additionally, CFD trading is inherently risky: so don't get rekt</Text>
                    </ModalBody>
                    <ModalFooter>
                        <HStack>
                            <Checkbox
                                isChecked={hideDisclaimerCheckbox}
                                onChange={(e) => {
                                    setHideDisclaimerCheckbox(e.target.checked);
                                }}
                            >
                                Don't show this dialog again.
                            </Checkbox>
                            <Button
                                colorScheme="blue"
                                mr={3}
                                onClick={() => {
                                    setHideDisclaimer(hideDisclaimerCheckbox);
                                    setAck(true);
                                }}
                            >
                                Dismiss
                            </Button>
                        </HStack>
                    </ModalFooter>
                </ModalContent>
            </Modal>
        </>
    );
}
