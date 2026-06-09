use crate::StfError;
use crate::{Account, Deposit, Transaction, TransactionResult, apply_deposit, apply_transaction};
use proptest::prelude::*;

impl Arbitrary for Account {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (any::<[u8; 32]>(), any::<u64>(), any::<u64>())
            .prop_map(|(id, balance, nonce)| Account { id, balance, nonce })
            .boxed()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn transaction_should_be_consistent(
        mut from in any::<Account>(),
        to in any::<Account>(),
        amount in any::<u64>(),
        fee in any::<u64>(),
    ) {
        if from.id == to.id {
            from.id[0] = from.id[0].wrapping_add(1);
        }

        let tx = Transaction {
            from: from.id,
            to: to.id,
            nonce: from.nonce,
            amount,
            fee,
        };

        let initial_from_balance = from.balance;
        let initial_to_balance = to.balance;

        let total_debit = match tx.amount.checked_add(tx.fee) {
            Some(val) => val,
            None => {
                let result = apply_transaction(from, to, &tx);
                prop_assert!(matches!(result, Err(StfError::BalanceOverflow)));
                return Ok(());
            }
        };

        if initial_from_balance >= total_debit {
            if initial_to_balance.checked_add(tx.amount).is_none() {
                let result = apply_transaction(from, to, &tx);
                prop_assert!(result.is_err(), "expected balance overflow for recipient");
                return Ok(());
            }

            let tx_result = apply_transaction(from.clone(), to, &tx).unwrap();

            if let TransactionResult::Success(updated_from, updated_to) = tx_result {
                prop_assert_eq!(updated_from.balance, initial_from_balance - total_debit, "from balance mismatch on success");
                prop_assert_eq!(updated_from.nonce, from.nonce + 1, "from nonce mismatch on success");
                prop_assert_eq!(updated_to.balance, initial_to_balance + tx.amount, "to balance mismatch on success");
            } else {
                panic!("expected success, got failure");
            }
        } else {
            let tx_result = apply_transaction(from.clone(), to, &tx).unwrap();
            if let TransactionResult::Failure(updated_from) = tx_result {
                let expected_balance = initial_from_balance.saturating_sub(tx.fee);
                prop_assert_eq!(updated_from.balance, expected_balance, "from balance mismatch on failure");
                prop_assert_eq!(updated_from.nonce, from.nonce + 1, "from nonce mismatch on failure");
            } else {
                panic!("expected failure, got success");
            }
        }
    }

    #[test]
    fn deposit_should_be_consistent(
        account in any::<Account>(),
        amount in any::<u64>(),
    ) {
        let deposit = Deposit {
            to: account.id,
            amount,
            timestamp: 0,
        };

        let initial_nonce = account.nonce;

        let result = apply_deposit(account.clone(), &deposit);

        match account.balance.checked_add(amount) {
            Some(expected_balance) => {
                let updated_account = result.unwrap();
                prop_assert_eq!(updated_account.balance, expected_balance, "balance mismatch on deposit success");
                prop_assert_eq!(updated_account.nonce, initial_nonce, "nonce should not change on deposit");
            }
            None => {
                prop_assert!(result.is_err(), "expected overflow error, got success");
            }
        }
    }
}
